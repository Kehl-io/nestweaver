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
use notify::{Event, EventKind};

/// Opaque caller-owned protection held for one watcher mutation window.
///
/// Daemon callers use this to compose their shutdown admission guard and
/// single-writer gate. Direct callers omit the factory because opening the
/// store already gives them exclusive writer ownership.
pub trait WatchMutationLease: Send {}

impl<T: Send> WatchMutationLease for T {}

pub type WatchMutationLeaseFactory =
    Arc<dyn Fn(&'static str) -> Result<Box<dyn WatchMutationLease>, anyhow::Error> + Send + Sync>;

/// Resolve the manifest-cache key for a watched package directory.
///
/// Match on the indexed working-tree location (`Repo::local_root`), not the
/// identity URL, so a repo whose path changed between indexes still updates
/// the same UID entry once `root_path` has been moved with it.
fn manifest_cache_repo_uid(store: &GraphStore, repo_path: &Path) -> Option<String> {
    let wanted = std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    store
        .list_repos(None)
        .ok()?
        .into_iter()
        .filter_map(|repo| {
            let root = repo.local_root()?;
            let rooted = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
            wanted
                .starts_with(&rooted)
                .then_some((rooted.components().count(), repo.uid))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, uid)| uid)
}

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
    "build.gradle.kts",
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

/// Per-phase wall-clock timings for one `process_batch` call. Logged as
/// structured `tracing` fields on the "BrainWatcher batch complete" event
/// (nw-475, Task 5.2), mirroring the daemon-boot phase breakdown
/// (`boot_ms`/`store_open_ms`/`extension_reconcile_ms`/... in
/// `nestweaver-daemon/src/server.rs`, nw-119): break a composite duration
/// into attributable phases on the SAME event, rather than leaving a reader
/// to guess which phase of a slow batch dominated. Zero for a phase that a
/// non-graph batch never runs.
#[derive(Debug, Default, Clone, Copy)]
struct BatchPhaseTimings {
    build_symbol_index_ms: u64,
    embedding_candidates_ms: u64,
    refresh_watched_paths_ms: u64,
    sidecars_ms: u64,
    tombstones_ms: u64,
    /// The non-graph-event loop (tag/manifest/other non-note filesystem
    /// events in the same debounced batch). Runs regardless of
    /// `graph_batch`, unlike the five phases above.
    non_graph_events_ms: u64,
    finalize_ms: u64,
    /// nw-668: the sidecar phase split into its parts, so a slow one names
    /// its culprit. `sidecar_scan_ms` runs with no lease held;
    /// `sidecar_flush_ms` and `sidecar_tantivy_ms` under it.
    sidecar_scan_ms: u64,
    sidecar_flush_ms: u64,
    sidecar_tantivy_ms: u64,
    /// Cross-domain edges the scan produced for this batch.
    sidecar_edges: usize,
    /// Write transactions and statements the cross-domain flush cost.
    sidecar_transactions: usize,
    sidecar_statements: usize,
    /// Changed notes still in the graph whose text this batch did not read.
    /// Reachable only for an edit that pushed a note over the size limit
    /// (nw-469): the refresh skips it without replacing its nodes, so it
    /// keeps its stored body AND the code links that body produced. (A read
    /// or parse failure of an indexed note fails the refresh instead.)
    sidecar_scan_skipped: usize,
    /// Flush attempts that failed and rolled back (non-fatal; logged). A
    /// chunk is retried once; one that fails twice is owed (`SidecarDebt`).
    sidecar_flush_failures: usize,
    /// nw-668: parameterized statements the flushes executed, counted at the
    /// store's execute boundary rather than taken from the flush's report.
    sidecar_boundary_statements: u64,
}

/// nw-653: backoff for retrying a failed startup reconciliation. Shared with
/// the code watcher's (nw-664), which retries on the same schedule.
pub(crate) const RECONCILE_RETRY_BASE: Duration = Duration::from_secs(5);
pub(crate) const RECONCILE_RETRY_CAP: Duration = Duration::from_secs(300);

/// nw-653 / nw-664: the delay before retry `failures` (1-based) of a startup
/// reconciliation: `base` doubling per failure, capped at
/// [`RECONCILE_RETRY_CAP`]. One schedule for the vault and code watchers.
pub(crate) fn reconcile_retry_delay(base: Duration, failures: u32) -> Duration {
    base.saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(RECONCILE_RETRY_CAP)
}

/// nw-653: a startup reconciliation that has not yet committed. It is kept and
/// retried from the event loop rather than dropped: dropping it is exactly how
/// the notes this fixes were lost. `paths` is `None` when the drift itself
/// could not be computed, so the retry recomputes it.
pub(crate) struct PendingReconciliation {
    pub(crate) paths: Option<Vec<PathBuf>>,
    /// Vault watcher only (nw-668): notes whose text committed but whose
    /// code links did not, replayed ON TOP of a recomputed drift — drift
    /// compares disk with the graph's text and cannot see them. Always empty
    /// when `paths` is `Some` (they are merged in) and for the code watcher.
    pub(crate) also_replay: Vec<PathBuf>,
    pub(crate) failures: u32,
    pub(crate) next_attempt: Instant,
}

/// nw-668: notes a batch committed whose cross-domain links did not land —
/// the flush failed, and so did its one immediate retry. The refresh already
/// replaced these notes' nodes, so they carry NO code links; the watcher owes
/// them a replay through the reconciliation retry.
#[derive(Debug)]
pub(crate) struct SidecarDebt {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) error: String,
}

#[cfg(test)]
thread_local! {
    /// Test failpoint (nw-668): fail this many upcoming note-structure reads
    /// in the sidecar phase.
    static FAIL_NOTE_STRUCTURE_READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A committed note's sections, and its headings when `with_headings`, for
/// the sidecar phase.
fn read_note_structure(
    store: &GraphStore,
    uid: &str,
    with_headings: bool,
) -> Result<
    (
        Vec<nestweaver_schema::Section>,
        Vec<nestweaver_schema::Heading>,
    ),
    anyhow::Error,
> {
    #[cfg(test)]
    if FAIL_NOTE_STRUCTURE_READS.with(|fail| {
        let armed = fail.get();
        fail.set(armed.saturating_sub(1));
        armed > 0
    }) {
        anyhow::bail!("injected note structure read failure");
    }
    let sections = store.sections_in_note(uid)?;
    let headings = if with_headings {
        store.headings_in_note(uid)?
    } else {
        Vec::new()
    };
    Ok((sections, headings))
}

/// Add `paths` to the batch's [`SidecarDebt`], keeping the first error.
fn owe(debt: &mut Option<SidecarDebt>, paths: &[PathBuf], error: &str) {
    debt.get_or_insert_with(|| SidecarDebt {
        paths: Vec::new(),
        error: error.to_string(),
    })
    .paths
    .extend(paths.iter().cloned());
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
    /// `[indexing].max_note_bytes`. Defaults to 1 MiB so tests and unconfigured
    /// watchers match the markdown indexer.
    note_limits: crate::index_limits::NoteLimits,
    /// nw-673: the instance's `[cross_domain]` settings (stoplist, minimum
    /// name length) for the watcher's own link scan, so an edited note is
    /// linked by the same rules the reconciler and the bulk pass apply.
    cross_domain: crate::config::CrossDomainConfig,
    /// Pre-opened TantivyIndex from the caller (e.g. daemon). When set,
    /// `run_inner` uses this instead of opening its own from `tantivy_path`.
    external_tantivy: Option<Arc<TantivyIndex>>,
    mutation_lease_factory: Option<WatchMutationLeaseFactory>,
    ready_callback: Option<Box<dyn FnOnce() + Send>>,
    /// nw-653: first delay before retrying a failed startup reconciliation;
    /// doubles per failure up to [`RECONCILE_RETRY_CAP`].
    reconcile_retry_base: Duration,
    #[cfg(test)]
    ready_signal: Option<std::sync::mpsc::Sender<()>>,
    /// Last batch's phase timings, for tests to assert on directly instead
    /// of re-parsing `tracing` output (nw-475, Task 5.2). Interior
    /// mutability because `process_batch` takes `&self`.
    #[cfg(test)]
    last_batch_phase_timings: std::sync::Mutex<Option<BatchPhaseTimings>>,
    #[cfg(test)]
    /// nw-668 test probe, called before each note's cross-domain scan with
    /// how many scanned-but-unflushed notes are held at that moment.
    sidecar_scan_probe: Option<Box<dyn Fn(usize) + Send + Sync>>,
    /// Test seam: subscribe as inotify would fail on an unreadable subtree.
    #[cfg(test)]
    emulate_inotify_watch: bool,
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
        note_paths: &[String],
    ) -> Result<
        nestweaver_store::IndexPublicationLease<'a>,
        crate::index::DeletionReconciliationError,
    > {
        let lease = crate::index::establish_index_publication_marker_with_io(
            store,
            Some(&self.db_path),
            nestweaver_store::index_publication::MARKER_REASON_WATCHER_BATCH,
            io,
        )?;
        // nw-475 (Task 5.2, owner decision Q7): stamp the watcher-batch
        // reason and the in-flight note paths onto the just-established
        // marker so ranked reads can recognize this window and serve with
        // disclosure instead of failing closed
        // (`GraphStore::index_publication_blocks_ranking`), and so the
        // disclosure can name the paths. `establish_marker` itself always
        // writes a plain `{pid}:{nanos}` payload (shared by every publisher,
        // not just the watcher), so the reason/paths are added in a second,
        // immediately-following write — the same pattern
        // `finalize_committed_index_for_scope_with_io` already uses to stamp
        // `MARKER_REASON_CANCELLED` after the fact. Best-effort: a write
        // failure here leaves the ordinary payload, which still fails closed
        // correctly, just without the watcher exception.
        let marker_path = crate::sidecar_path(&self.db_path, ".index-dirty");
        let payload = nestweaver_store::index_publication::format_marker_payload_with_note_paths(
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            Some(nestweaver_store::index_publication::MARKER_REASON_WATCHER_BATCH),
            note_paths,
        );
        if let Err(error) =
            nestweaver_store::durable_sidecar::atomic_replace_file(&marker_path, |file| {
                std::io::Write::write_all(file, payload.as_bytes())
            })
        {
            tracing::warn!(
                "could not record the watcher-batch reason/paths in {}: {error:#}; ranked \
                 reads will fail closed instead of serving with disclosure for this batch",
                marker_path.display()
            );
        }
        Ok(lease)
    }

    fn finalize_graph_publication_with_io(
        &self,
        publication: nestweaver_store::IndexPublicationLease<'_>,
        io: &dyn crate::index::IndexEpilogueIo,
    ) -> Result<(), crate::index::DeletionReconciliationError> {
        crate::index::finalize_committed_index_for_scope_with_io(
            publication,
            Some(&self.db_path),
            nestweaver_store::index_publication::MARKER_REASON_WATCHER_BATCH,
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
            note_limits: crate::index_limits::NoteLimits::default(),
            external_tantivy: None,
            cross_domain: crate::config::CrossDomainConfig::default(),
            mutation_lease_factory: None,
            ready_callback: None,
            reconcile_retry_base: RECONCILE_RETRY_BASE,
            #[cfg(test)]
            ready_signal: None,
            #[cfg(test)]
            last_batch_phase_timings: std::sync::Mutex::new(None),
            #[cfg(test)]
            sidecar_scan_probe: None,
            #[cfg(test)]
            emulate_inotify_watch: false,
        }
    }

    /// nw-651 on Linux: disclose the directories the subscription could not
    /// cover as the vault walk's unreadable-directory rows, minus `SKIP_DIRS`
    /// (never walked) — `.brainignore` is applied by the writer. A directory
    /// that became watchable again needs no call: the batch that follows
    /// walks the vault and re-derives every walk row.
    fn disclose_unwatchable_dirs(&self, store: &GraphStore, dirs: &[(PathBuf, String)]) {
        let dirs: Vec<(PathBuf, String)> = dirs
            .iter()
            .filter(|(dir, _)| !path_in_skip_dir(dir))
            .cloned()
            .collect();
        for (dir, error) in &dirs {
            tracing::warn!(
                dir = %dir.display(),
                %error,
                "brain watcher cannot watch this directory; edits beneath it are not seen live"
            );
        }
        crate::index_md::disclose_unwatchable_vault_dirs(
            store.db_path(),
            &self.vault_root,
            &self.ignore_set,
            &dirs,
        );
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

    /// Enable Tantivy index sync. When set, every note update/delete
    /// also updates the BM25 index at this path. Leave unset to skip
    /// Tantivy maintenance (graph stays current but BM25 search falls
    /// behind until `brain reindex-search`).
    pub fn with_tantivy_index(mut self, path: impl Into<PathBuf>) -> Self {
        self.tantivy_path = Some(path.into());
        self
    }

    /// Apply the configured markdown-note size limit to watched vault reads.
    pub fn with_note_limits(mut self, limits: crate::index_limits::NoteLimits) -> Self {
        self.note_limits = limits;
        self
    }

    /// Apply the instance's `[cross_domain]` settings to the link scan
    /// (nw-673). Unset, the built-in defaults apply.
    pub fn with_cross_domain_config(mut self, config: crate::config::CrossDomainConfig) -> Self {
        self.cross_domain = config;
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
            && (path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| MANIFEST_FILES.contains(&name))
                || path.extension().is_some_and(|s| s == "csproj"))
    }

    /// Test hook: shorten the startup-reconciliation retry backoff (nw-653).
    #[cfg(test)]
    fn with_reconcile_retry_base(mut self, base: Duration) -> Self {
        self.reconcile_retry_base = base;
        self
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

    /// The phase timings `process_batch` recorded on its most recent call,
    /// for tests. `None` before any batch has run.
    #[cfg(test)]
    fn last_batch_phase_timings(&self) -> Option<BatchPhaseTimings> {
        *self
            .last_batch_phase_timings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            .with_context(|| "init filesystem watcher")?;
        // nw-651 on Linux: an unreadable subdirectory must not stop the whole
        // subscription; it is skipped and disclosed instead (`watch_tree`).
        #[cfg(test)]
        let tree = if self.emulate_inotify_watch {
            crate::watch_tree::TreeWatch::start_emulating_inotify(watcher, &self.vault_root)
        } else {
            crate::watch_tree::TreeWatch::start(watcher, &self.vault_root)
        };
        #[cfg(not(test))]
        let tree = crate::watch_tree::TreeWatch::start(watcher, &self.vault_root);
        let mut tree = tree.with_context(|| format!("watch {}", self.vault_root.display()))?;
        self.disclose_unwatchable_dirs(&store, tree.unwatchable());
        let mut subscribed_unwatchable = tree.unwatchable().to_vec();
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
        // nw-608: before publishing the Vault node below.
        crate::vault_registration::refuse_duplicate_vault_name(
            &store,
            &v_uid,
            &self.vault_name,
            &self.vault_root,
        )?;
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
            &[],
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
        // nw-653: replay what changed while no watcher was listening. The
        // notify subscription above is already live, so a save racing this
        // scan is queued and reprocessed by the loop; overlap is harmless.
        // Runs before readiness so "ready" normally means the graph matches
        // disk. A failure does not stop live watching (the supervisor would
        // only restart into the same failure): it is disclosed in the
        // skipped-notes status and retried from the loop below with backoff.
        let mut pending = match self.attempt_reconciliation(
            &store,
            tantivy.as_deref(),
            &v_uid,
            &on_change,
            None,
            // nw-668: notes a previous run still owed (their text landed,
            // their code links did not) — invisible to the drift below.
            store
                .db_path()
                .map(|db| crate::index_md::vault_owed_note_paths(db, &self.vault_root))
                .unwrap_or_default(),
            0,
        ) {
            Ok(pending) => pending,
            Err(error) => {
                tracing::info!(
                    "BrainWatcher startup reconciliation refused during shutdown; exiting: {error:#}"
                );
                return Ok(());
            }
        };
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
            // nw-653: retry an owed startup reconciliation once its backoff
            // elapses. Checked every iteration, so an idle vault retries on
            // the receive timeout tick and a busy one between batches.
            if let Some(owed) = pending.take_if(|owed| Instant::now() >= owed.next_attempt) {
                match self.attempt_reconciliation(
                    &store,
                    tantivy.as_deref(),
                    &v_uid,
                    &on_change,
                    owed.paths,
                    owed.also_replay,
                    owed.failures,
                ) {
                    Ok(next) => pending = next,
                    Err(_) => return Ok(()),
                }
            }
            let mut batch = match receive_debounced_paths(
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

            // nw-651 on Linux: a directory created under a non-recursively
            // watched one needs its own watch, and its notes may predate it.
            let adopted = tree.adopt_new_dirs(&batch);
            batch.extend(adopted);
            if tree.unwatchable() != subscribed_unwatchable.as_slice() {
                subscribed_unwatchable = tree.unwatchable().to_vec();
                self.disclose_unwatchable_dirs(&store, &subscribed_unwatchable);
            }
            match self.process_batch(&store, tantivy.as_deref(), &v_uid, batch, &on_change) {
                Ok(None) => {}
                // nw-668: committed text, missing code links — owed, disclosed
                // and retried on the reconciliation backoff, not just logged.
                Ok(Some(debt)) => {
                    pending = Some(self.owe_sidecar_retry(&store, pending.take(), debt))
                }
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
    ) -> Result<Option<SidecarDebt>, anyhow::Error> {
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

        // Computed up front (pure filter over `unique_paths`, no side
        // effects) so both the publication marker below (nw-475, Task 5.2:
        // the marker records these as its in-flight note paths) and the
        // per-file work later can use it.
        let graph_paths: Vec<_> = unique_paths
            .iter()
            .filter(|path| self.event_targets_graph(path))
            .cloned()
            .collect();
        // Vault-relative, for a marker payload disclosure meant for a human
        // or agent — not the host's absolute filesystem layout.
        let graph_note_paths: Vec<String> = graph_paths
            .iter()
            .map(|path| {
                path.strip_prefix(&self.vault_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        // nw-380: establishing the fail-closed publication marker is itself
        // a store write, so it gets its own freshly acquired lease rather
        // than inheriting one held since before this batch began — there is
        // no reason to hold the gate across the symbol-index/title-map
        // rebuild below, which touches no shared derived state the gate
        // protects. Index write-boundary acquire then YIELDS the write gate
        // if this publication lease is still held, so finalize's later
        // write-gate re-acquire cannot AB-BA deadlock with `index_repo`.
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
                &graph_note_paths,
            )?)
        } else {
            None
        };

        // nw-475 (Task 5.2): each named phase gets its own timer so a slow
        // batch is diagnosable by WHICH phase dominated, not just that the
        // whole batch was slow — mirrors the daemon-boot phase breakdown
        // (`boot_ms`/`store_open_ms`/`extension_reconcile_ms`/...,
        // server.rs, nw-119). All fields are reported together on the single
        // "BrainWatcher batch complete" event below. A non-graph batch never
        // runs the phases gated on `graph_batch`, so those stay at their
        // zero default.
        let mut phase_timings = BatchPhaseTimings::default();

        let build_symbol_index_started = Instant::now();
        let symbol_index =
            crate::cross_domain::build_symbol_index_with_config(store, &self.cross_domain).ok();
        phase_timings.build_symbol_index_ms =
            build_symbol_index_started.elapsed().as_millis() as u64;

        let mut batch_failures = Vec::new();
        let mut sidecar_debt = None;
        if graph_batch {
            let embedding_candidates_started = Instant::now();
            let mut embedding_candidates = Vec::new();
            for path in &graph_paths {
                let relative = path.strip_prefix(&self.vault_root)?;
                embedding_candidates.extend(store.note_embedding_candidate_uids(&note_uid(
                    v_uid,
                    &relative.to_string_lossy(),
                ))?);
            }
            phase_timings.embedding_candidates_ms =
                embedding_candidates_started.elapsed().as_millis() as u64;

            // Planning parses changed notes and affected linking sources before
            // any deletion. A failed plan or transaction leaves the publication
            // marker dirty and never emits a successful change callback.
            let refresh_watched_paths_started = Instant::now();
            let committed_sources = crate::index_md::refresh_watched_paths(
                store,
                &self.vault_root,
                &self.instance_id,
                &self.vault_name,
                &graph_paths,
                &self.ignore_set,
                self.note_limits,
                &|| self.try_acquire_batch_lease(true, "watch_vault_batch"),
            )?;
            phase_timings.refresh_watched_paths_ms =
                refresh_watched_paths_started.elapsed().as_millis() as u64;

            let note_tags: HashMap<_, _> = store.note_tag_sets()?.into_iter().collect();
            let sidecars_started = Instant::now();
            sidecar_debt = self.refresh_prepared_sidecars(
                store,
                tantivy,
                v_uid,
                &graph_paths,
                symbol_index.as_ref(),
                &note_tags,
                &committed_sources,
                &mut phase_timings,
            )?;
            phase_timings.sidecars_ms = sidecars_started.elapsed().as_millis() as u64;

            let tombstones_started = Instant::now();
            let _lease = self.try_acquire_batch_lease(true, "watch_vault_embeddings")?;
            tombstone_vault_embeddings_after_commit(store, &embedding_candidates, "watched batch");
            phase_timings.tombstones_ms = tombstones_started.elapsed().as_millis() as u64;
        }
        let non_graph_events_started = Instant::now();
        for path in unique_paths
            .into_iter()
            .filter(|path| !self.event_targets_graph(path))
        {
            let _lease = self.try_acquire_batch_lease(mutation_batch, "watch_vault_batch")?;
            let outcome = self.handle_non_graph_event(store, path)?;
            log_outcome(&outcome);
        }
        phase_timings.non_graph_events_ms = non_graph_events_started.elapsed().as_millis() as u64;

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
        // "~milliseconds".) `finalize_ms` covers this recompute together
        // with the publication finalize itself — one lease-held window.
        if graph_batch {
            let finalize_started = Instant::now();
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
            phase_timings.finalize_ms = finalize_started.elapsed().as_millis() as u64;
        }
        // nw-380: "instrument first" — nothing previously logged batch size
        // or hold duration, so a recurrence could not distinguish "many
        // files" from "one slow file" from "an expensive PPR recompute" as
        // the cause. `elapsed_ms` covers the WHOLE batch (all per-file lease
        // windows plus the final publish), not any single lease hold,
        // precisely because per-file holds are no longer expected to
        // dominate it after this fix. nw-475 (Task 5.2) adds the named phase
        // fields alongside it so a slow batch is attributable to a specific
        // phase without re-instrumenting later.
        tracing::info!(
            files = batch_len,
            elapsed_ms = batch_started.elapsed().as_millis() as u64,
            mutation_batch,
            build_symbol_index_ms = phase_timings.build_symbol_index_ms,
            embedding_candidates_ms = phase_timings.embedding_candidates_ms,
            refresh_watched_paths_ms = phase_timings.refresh_watched_paths_ms,
            sidecars_ms = phase_timings.sidecars_ms,
            sidecar_scan_ms = phase_timings.sidecar_scan_ms,
            sidecar_flush_ms = phase_timings.sidecar_flush_ms,
            sidecar_tantivy_ms = phase_timings.sidecar_tantivy_ms,
            sidecar_edges = phase_timings.sidecar_edges,
            sidecar_transactions = phase_timings.sidecar_transactions,
            sidecar_statements = phase_timings.sidecar_statements,
            sidecar_scan_skipped = phase_timings.sidecar_scan_skipped,
            sidecar_flush_failures = phase_timings.sidecar_flush_failures,
            tombstones_ms = phase_timings.tombstones_ms,
            non_graph_events_ms = phase_timings.non_graph_events_ms,
            finalize_ms = phase_timings.finalize_ms,
            "BrainWatcher batch complete"
        );
        #[cfg(test)]
        {
            *self
                .last_batch_phase_timings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(phase_timings);
        }
        if !batch_failures.is_empty() {
            anyhow::bail!(
                "brain watcher batch failed after committed graph work: {}",
                batch_failures.join("; ")
            );
        }
        Ok(sidecar_debt)
    }

    /// nw-653: one reconciliation attempt. `Ok(None)` means the graph now
    /// matches disk and any disclosed debt was cleared; `Ok(Some(_))` means it
    /// failed, was disclosed in the skipped-notes status, and is owed a retry.
    /// `Err` only for a shutdown refusal, which ends the watcher.
    #[allow(clippy::too_many_arguments)]
    fn attempt_reconciliation(
        &self,
        store: &GraphStore,
        tantivy: Option<&TantivyIndex>,
        v_uid: &str,
        on_change: &Option<Box<dyn Fn() + Send>>,
        known: Option<Vec<PathBuf>>,
        also_replay: Vec<PathBuf>,
        failures: u32,
    ) -> Result<Option<PendingReconciliation>, anyhow::Error> {
        let drift = match known {
            Some(paths) => Ok(paths),
            None => crate::index_md::vault_startup_drift(
                store,
                &self.vault_root,
                &self.instance_id,
                &self.ignore_set,
                self.note_limits,
            ),
        }
        .map(|mut paths| {
            paths.extend(also_replay.iter().cloned());
            paths.sort();
            paths.dedup();
            paths
        });
        // nw-668: a replay whose text landed but whose code links did not is
        // disclosed as owed links, not as a failed startup reconciliation.
        let mut link_failure = false;
        let (paths, error) = match drift {
            Ok(paths) if paths.is_empty() => {
                // Nothing left to reconcile: settle any debt an earlier
                // attempt disclosed (including an uncomputable drift), or
                // status reports it pending forever.
                crate::index_md::record_vault_reconciliation_debt(
                    store.db_path(),
                    &self.vault_root,
                    &[],
                    None,
                );
                return Ok(None);
            }
            Ok(paths) => {
                tracing::info!(
                    vault = %self.vault_root.display(),
                    notes = paths.len(),
                    "BrainWatcher startup: reconciling notes changed while no watcher ran"
                );
                match self.process_batch(store, tantivy, v_uid, paths.clone(), on_change) {
                    // nw-668: the text landed but some notes' code links did
                    // not; only those are still owed.
                    Ok(Some(debt)) => {
                        link_failure = true;
                        (Some(debt.paths), anyhow::anyhow!(debt.error))
                    }
                    Ok(None) => {
                        crate::index_md::record_vault_reconciliation_debt(
                            store.db_path(),
                            &self.vault_root,
                            &paths,
                            None,
                        );
                        return Ok(None);
                    }
                    Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                        return Err(error);
                    }
                    Err(error) => (Some(paths), error),
                }
            }
            Err(error) => (None, error),
        };
        let failures = failures.saturating_add(1);
        let delay = reconcile_retry_delay(self.reconcile_retry_base, failures);
        let message = format!("{error:#}");
        if link_failure {
            tracing::error!(
                vault = %self.vault_root.display(),
                error = %message,
                failures,
                retry_in_ms = delay.as_millis() as u64,
                "BrainWatcher reconciled the notes' text but could not write their code \
                 links; they have none until the retry lands (retrying; `nestweaver brain \
                 refresh` also heals it)"
            );
        } else {
            tracing::error!(
                vault = %self.vault_root.display(),
                error = %message,
                failures,
                retry_in_ms = delay.as_millis() as u64,
                "BrainWatcher startup reconciliation failed; notes changed while no watcher \
                 ran are missing or stale until it succeeds (retrying; `nestweaver brain \
                 refresh` also heals it)"
            );
        }
        // An uncomputable drift is disclosed against the vault root itself,
        // plus any owed links it must still replay on top (nw-668).
        let also_replay = if paths.is_some() {
            Vec::new()
        } else {
            also_replay
        };
        let mut disclosed = paths
            .clone()
            .unwrap_or_else(|| vec![self.vault_root.clone()]);
        disclosed.extend(also_replay.iter().cloned());
        if link_failure {
            crate::index_md::record_vault_link_debt(
                store.db_path(),
                &self.vault_root,
                &disclosed,
                &message,
                true,
            );
        } else {
            crate::index_md::record_vault_reconciliation_debt(
                store.db_path(),
                &self.vault_root,
                &disclosed,
                Some(&message),
            );
        }
        Ok(Some(PendingReconciliation {
            paths,
            also_replay,
            failures,
            next_attempt: Instant::now() + delay,
        }))
    }

    /// nw-668: hand notes whose code links failed to land to the
    /// reconciliation retry, the same seam nw-653 uses for a failed startup
    /// replay: disclosed as owed in the skipped-notes status, replayed on the
    /// shared backoff, disclosure cleared when the replay lands. Merged into
    /// an already-owed retry, that retry keeps its schedule. A retry that
    /// recomputes drift (`paths: None`) cannot see these notes — their text
    /// matches disk — so they ride along in `also_replay`.
    fn owe_sidecar_retry(
        &self,
        store: &GraphStore,
        pending: Option<PendingReconciliation>,
        debt: SidecarDebt,
    ) -> PendingReconciliation {
        let merge = |mut owed: Vec<PathBuf>| {
            owed.extend(debt.paths.iter().cloned());
            owed.sort();
            owed.dedup();
            owed
        };
        let next = match pending {
            Some(owed) => match owed.paths {
                Some(known) => PendingReconciliation {
                    paths: Some(merge(known)),
                    ..owed
                },
                None => PendingReconciliation {
                    also_replay: merge(owed.also_replay),
                    ..owed
                },
            },
            None => PendingReconciliation {
                paths: Some(merge(Vec::new())),
                also_replay: Vec::new(),
                failures: 1,
                next_attempt: Instant::now() + reconcile_retry_delay(self.reconcile_retry_base, 1),
            },
        };
        tracing::error!(
            vault = %self.vault_root.display(),
            error = %debt.error,
            notes = debt.paths.len(),
            "BrainWatcher could not write code links for committed notes; they have none \
             until the retry lands (retrying; `nestweaver brain refresh` also heals it)"
        );
        // Merged, not replaced: rows an owed startup replay already disclosed
        // keep their own (startup) reason.
        crate::index_md::record_vault_link_debt(
            store.db_path(),
            &self.vault_root,
            &debt.paths,
            &debt.error,
            false,
        );
        next
    }

    /// nw-653: diff disk against the graph (`index_md::vault_startup_drift`)
    /// and push the drifted paths through the ordinary batch seam, so lost
    /// creates, edits and deletes get the same publication marker, BM25,
    /// embedding and PageRank maintenance as a live event. Returns how many
    /// paths were replayed; zero means no batch ran.
    #[cfg(test)]
    fn reconcile_startup_drift(
        &self,
        store: &GraphStore,
        tantivy: Option<&TantivyIndex>,
        v_uid: &str,
        on_change: &Option<Box<dyn Fn() + Send>>,
    ) -> Result<usize, anyhow::Error> {
        let drift = crate::index_md::vault_startup_drift(
            store,
            &self.vault_root,
            &self.instance_id,
            &self.ignore_set,
            self.note_limits,
        )?;
        if drift.is_empty() {
            return Ok(0);
        }
        let replayed = drift.len();
        tracing::info!(
            vault = %self.vault_root.display(),
            notes = replayed,
            "BrainWatcher startup: reconciling notes changed while no watcher ran"
        );
        if let Some(debt) = self.process_batch(store, tantivy, v_uid, drift, on_change)? {
            anyhow::bail!(debt.error);
        }
        Ok(replayed)
    }

    /// Refresh the cross-domain edges and BM25 docs of every note in a
    /// committed batch, mirroring the committed note representation rather
    /// than reading the files again: another save can occur while this
    /// prepared batch is committing.
    ///
    /// nw-668: this used to take the `watch_vault_sidecars` lease once per
    /// note and, under it, re-read the file from disk and write every
    /// note→symbol and section→symbol edge as its own auto-committed
    /// statement — 10K–100K commits for one edited note, holding the write
    /// lease for up to 22 minutes while reconcilers queued behind it. Now,
    /// chunk by chunk of [`crate::cross_domain::NOTES_PER_TXN`] notes:
    ///
    /// 1. SCAN with no lease held: tokenise each changed note's committed
    ///    text (`sources`, from the refresh that just committed it) against
    ///    the symbol index. Only one chunk's scanned edges are ever held, so
    ///    a large startup replay of heavily linked notes stays bounded.
    /// 2. FLUSH the chunk under its own lease: one transaction, set-based
    ///    statements. Releasing between chunks keeps the gate FIFO-fair.
    /// 3. Tantivy docs (text-sized) are gathered along the way and committed
    ///    ONCE for the batch, deletions included, under the last lease.
    ///
    /// A failed flush rolls back whole — never a partial edge set — and is
    /// retried once at once. It cannot fall back to the notes' PREVIOUS
    /// edges: the refresh that just committed them replaced each changed
    /// Note and its Sections (`delete_note_cascade_on` DETACH-deletes them,
    /// edges included). So a chunk that fails twice leaves its notes with NO
    /// code links, and they are returned as a [`SidecarDebt`] for the caller
    /// to owe, disclose and replay. It stays non-fatal to the batch: the
    /// notes' text is committed and still publishes.
    #[allow(clippy::too_many_arguments)]
    fn refresh_prepared_sidecars(
        &self,
        store: &GraphStore,
        tantivy: Option<&TantivyIndex>,
        v_uid: &str,
        paths: &[PathBuf],
        symbols: Option<&crate::cross_domain::SymbolIndex>,
        note_tags: &HashMap<String, Vec<String>>,
        sources: &HashMap<PathBuf, String>,
        timings: &mut BatchPhaseTimings,
    ) -> Result<Option<SidecarDebt>, anyhow::Error> {
        let setup_started = Instant::now();
        let mut targets = Vec::with_capacity(paths.len());
        for path in paths {
            let relative = path.strip_prefix(&self.vault_root)?;
            let uid = note_uid(v_uid, &relative.to_string_lossy());
            targets.push((path, relative.to_path_buf(), uid));
        }
        let uids: Vec<String> = targets.iter().map(|(_, _, uid)| uid.clone()).collect();
        let notes: HashMap<String, nestweaver_schema::Note> = store
            .lookup_notes_by_uids(&uids)?
            .into_iter()
            .map(|note| (note.uid.clone(), note))
            .collect();
        let built_index;
        let index = match symbols {
            Some(index) => Some(index),
            None => {
                match crate::cross_domain::build_symbol_index_with_config(store, &self.cross_domain)
                {
                    Ok(index) => {
                        built_index = index;
                        Some(&built_index)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "watcher cross-domain refresh failed");
                        None
                    }
                }
            }
        }
        // No code indexed: nothing to bridge to, and existing edges are left
        // alone, as the per-note path did.
        .filter(|index| !index.is_empty());
        timings.sidecar_scan_ms += setup_started.elapsed().as_millis() as u64;

        let mut debt: Option<SidecarDebt> = None;
        let mut chunk: Vec<crate::cross_domain::ScannedNote> = Vec::new();
        let mut chunk_paths: Vec<PathBuf> = Vec::new();
        let mut removals = Vec::new();
        let mut removed_paths: Vec<PathBuf> = Vec::new();
        let mut docs: Vec<nestweaver_store::tantivy_index::NoteDocBatchEntry> = Vec::new();
        for (path, relative, uid) in &targets {
            // Flush a full chunk before scanning further, so at most one
            // chunk of scanned edges is ever held.
            if chunk.len() == crate::cross_domain::NOTES_PER_TXN {
                let _lease = self.try_acquire_batch_lease(true, "watch_vault_sidecars")?;
                self.flush_sidecar_chunk(store, &chunk, &chunk_paths, timings, &mut debt);
                chunk.clear();
                chunk_paths.clear();
            }
            let scan_started = Instant::now();
            let Some(note) = notes.get(uid) else {
                removals.push(uid.clone());
                removed_paths.push((*path).clone());
                continue;
            };
            // nw-668: a note whose structure cannot be read is owed — no
            // links, no Tantivy doc this batch — and replayed by the retry,
            // with or without Tantivy. It used to fail the whole batch (and,
            // live, end the run loop) when Tantivy was on. Nothing here gates
            // publication: the text is committed; only derived sidecars wait.
            let (sections, headings) = match read_note_structure(store, uid, tantivy.is_some()) {
                Ok(structure) => structure,
                Err(error) => {
                    tracing::warn!(%error, note = %uid, "watcher sidecar refresh failed");
                    owe(
                        &mut debt,
                        std::slice::from_ref(*path),
                        &format!("{error:#}"),
                    );
                    continue;
                }
            };
            #[cfg(test)]
            if let Some(probe) = &self.sidecar_scan_probe {
                probe(chunk.len());
            }
            if let Some(index) = index {
                match sources.get(relative) {
                    Some(source) => {
                        chunk.push(crate::cross_domain::scan_note_source(
                            uid, source, &sections, index,
                        ));
                        chunk_paths.push((*path).clone());
                    }
                    None => timings.sidecar_scan_skipped += 1,
                }
            }
            if tantivy.is_some() {
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
                let heading_docs: Vec<_> = headings.into_iter().map(|h| (h.uid, h.text)).collect();
                let mut body = Vec::new();
                if let Some(raw) = &note.frontmatter_raw {
                    body.push(raw.clone());
                }
                body.extend(sections.into_iter().map(|s| s.text_content));
                docs.push((
                    uid.clone(),
                    note.title.clone(),
                    v_uid.to_string(),
                    body,
                    heading_docs,
                    section_docs,
                    note_tags.get(uid).cloned().unwrap_or_default(),
                ));
            }
            timings.sidecar_scan_ms += scan_started.elapsed().as_millis() as u64;
        }

        let tantivy_work = tantivy.filter(|_| !docs.is_empty() || !removals.is_empty());
        if !chunk.is_empty() || tantivy_work.is_some() {
            let _lease = self.try_acquire_batch_lease(true, "watch_vault_sidecars")?;
            self.flush_sidecar_chunk(store, &chunk, &chunk_paths, timings, &mut debt);
            if let Some(index) = tantivy_work {
                let tantivy_started = Instant::now();
                index.apply_note_batch(&docs, &removals)?;
                timings.sidecar_tantivy_ms = tantivy_started.elapsed().as_millis() as u64;
            }
        }
        // A deleted note owes nothing any more.
        if let Some(db_path) = store.db_path() {
            crate::index_md::clear_vault_reconciliation_paths(
                db_path,
                &self.vault_root,
                &removed_paths,
            );
        }
        Ok(debt)
    }

    /// Flush one chunk of scanned notes (caller holds the lease), retrying
    /// once on failure; a chunk that fails twice is added to `debt`.
    fn flush_sidecar_chunk(
        &self,
        store: &GraphStore,
        chunk: &[crate::cross_domain::ScannedNote],
        chunk_paths: &[PathBuf],
        timings: &mut BatchPhaseTimings,
        debt: &mut Option<SidecarDebt>,
    ) {
        if chunk.is_empty() {
            return;
        }
        timings.sidecar_edges += chunk
            .iter()
            .map(crate::cross_domain::ScannedNote::edge_count)
            .sum::<usize>();
        let flush_started = Instant::now();
        let mut last_error = None;
        for _attempt in 0..2 {
            let mut result = crate::cross_domain::CrossDomainResult::default();
            let boundary = nestweaver_store::write::parameterized_writes_on_this_thread();
            let flushed = crate::cross_domain::flush_scanned_notes(store, chunk, &mut result);
            timings.sidecar_boundary_statements +=
                nestweaver_store::write::parameterized_writes_on_this_thread() - boundary;
            match flushed {
                Ok(stats) => {
                    timings.sidecar_transactions += stats.transactions;
                    timings.sidecar_statements += stats.statements;
                    // These notes' text and links have both landed: settle
                    // any owed disclosure now rather than at the next retry.
                    if let Some(db_path) = store.db_path() {
                        crate::index_md::clear_vault_reconciliation_paths(
                            db_path,
                            &self.vault_root,
                            chunk_paths,
                        );
                    }
                    last_error = None;
                    break;
                }
                Err(error) => {
                    timings.sidecar_flush_failures += 1;
                    tracing::warn!(
                        %error,
                        notes = chunk.len(),
                        "watcher cross-domain flush failed and rolled back; these notes \
                         have no code links until it lands"
                    );
                    last_error = Some(error);
                }
            }
        }
        timings.sidecar_flush_ms += flush_started.elapsed().as_millis() as u64;
        if let Some(error) = last_error {
            owe(debt, chunk_paths, &format!("{error:#}"));
        }
    }

    /// Advance and persist the graph generation for a manifest edit (nw-498),
    /// so responses cached against the old generation cannot outlive it.
    ///
    /// Returns whether the generation actually moved.
    ///
    /// Skipped while an index publication is dirty, and that is not a gap.
    /// A manifest edit only lands inside a publication window when the SAME
    /// debounced batch also touched the graph, and that batch's own finalize
    /// (`finalize_graph_publication_with_io`) advances and persists the
    /// generation for the whole window — so the invalidation still happens,
    /// once. Bumping here as well would be refused anyway: the publication
    /// holds a reserved successor generation and `try_bump_graph_generation`
    /// fails closed against a live reservation rather than clobbering it.
    /// Cached reads are already bypassed for the duration
    /// (`maybe_cached` dispatches uncached while publication is dirty), so
    /// nothing stale can be served out of the skipped window either.
    fn advance_generation_for_manifest_edit(&self, store: &GraphStore) -> bool {
        if store.is_index_publication_dirty() {
            return false;
        }
        let before = store.graph_generation();
        store.bump_and_persist_graph_generation(&crate::sidecar_path(&self.db_path, ".generation"));
        store.graph_generation() != before
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
            if is_manifest || path.extension().is_some_and(|s| s == "csproj") {
                let repo_path = path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf();
                if manifests_path == &crate::manifest::manifest_cache_path(&self.db_path) {
                    if manifest_cache_repo_uid(store, &repo_path).is_none() {
                        return Ok(UpdateOutcome::Skipped {
                            path,
                            reason: "manifest file — no indexed repo",
                        });
                    }
                    // Durable debt precedes invalidation, including deletion.
                    // A complete-map daemon job owns parsing and publication.
                    crate::manifest::mark_manifest_reconciliation_pending(
                        &self.db_path,
                        "manifest watcher edit",
                    )?;
                    self.advance_generation_for_manifest_edit(store);
                    return Ok(UpdateOutcome::Skipped {
                        path,
                        reason: "manifest file — reconciliation pending",
                    });
                }
                let manifest = crate::manifest::parse_manifest(
                    &crate::content_reader::FilesystemReader::new(&repo_path),
                );
                // nw-522: every other writer keys this map by repo UID.
                // A path key is treated as non-live by
                // `reconcile_deleted_graph_state` and cannot match
                // `Symbol::repo_uid` without a read-side remap. Skip the
                // insert when no indexed repo owns this directory — writing
                // a path would recreate the divergence this refresh exists
                // to close. A later index (or a root_path update) is what
                // makes the same directory resolvable again.
                let Some(repo_key) = manifest_cache_repo_uid(store, &repo_path) else {
                    tracing::debug!(
                        path = %repo_path.display(),
                        "watcher: manifest edit has no indexed repo; skipping cache insert"
                    );
                    return Ok(UpdateOutcome::Skipped {
                        path,
                        reason: "manifest file — no indexed repo",
                    });
                };
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
                        // nw-498: advance the graph generation BEFORE saving.
                        //
                        // The MCP response cache keys every hit on
                        // `graph_generation` (plus the filemeta scope digest),
                        // and neither moves for a manifest edit: a
                        // `package.json` is not a parsed source file, so it is
                        // in no filemeta slice, and this refresh never touched
                        // the counter. So a cached `dead_code` — whose entry
                        // points come from exactly this sidecar — kept being
                        // served from the pre-edit answer for as long as
                        // nothing else happened to reindex. Adding or removing
                        // a package entry point changes which symbols are
                        // reachable, and the stale side of that is a LIVE
                        // symbol still listed as dead.
                        //
                        // BEFORE the save, not after, because the canonical
                        // sidecar is generation-bound: its envelope records
                        // `source_graph_generation` and a later reader rejects
                        // it when that no longer matches. Saving at N and then
                        // advancing to N+1 would make a freshly written
                        // artifact stale on arrival — the exact ordering rule
                        // `finalize_code_graph_deletion` and
                        // `advancing_generation_rebinding_manifests` already
                        // record.
                        self.advance_generation_for_manifest_edit(store);
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
                            return Err(e.context(format!(
                                "watcher manifest save failed after {}",
                                path.display()
                            )));
                        } else {
                            tracing::info!(
                                repo = %repo_key,
                                manifest = %path.display(),
                                "watcher: manifest cache refreshed"
                            );
                        }
                    }
                    Err(e) => {
                        return Err(
                            e.context("watcher manifest load failed; cache was not refreshed")
                        );
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

/// nw-652: this list stays the vault's own (see [`SKIP_DIRS`]), but the MATCH
/// is the shared one, so a `target/` notes folder with no build manifest beside
/// it is watched exactly as the vault walk now indexes it. `path` is the
/// absolute event path, so the manifest probe is a plain stat.
fn path_in_skip_dir(path: &Path) -> bool {
    crate::index::path_in_skip_dirs(
        path,
        SKIP_DIRS,
        crate::index::nothing_unskipped(),
        &|probe| probe.is_file(),
    )
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
        // Match indexer identities and canonical filesystem notifications,
        // including macOS's /var -> /private/var temporary-directory alias.
        let root = fs::canonicalize(root).unwrap();
        for (rel, content) in files {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, content).unwrap();
        }
        (dir, root)
    }

    fn insert_watched_repo(store: &GraphStore, root: &Path, uid: &str) {
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: uid.to_string(),
                url: format!("file://{}", root.display()),
                indexed_sha: "test".into(),
                staleness_commits_behind: 0,
                instance_id: "test".into(),
                name: Some("watched-package".into()),
                root_path: Some(root.to_string_lossy().into_owned()),
            })
            .unwrap();
    }

    /// nw-608: `brain watch --name r4-vault` on a new root, against a copied
    /// database that already held `r4-vault` at its original root, published
    /// a SECOND same-named vault; `brain search` then returned both notes.
    /// The watcher must refuse at startup, before it publishes anything.
    #[test]
    fn watching_a_new_root_under_an_existing_vault_name_is_refused() {
        let _guard = serial_watcher_test();
        let (_original_dir, original) = make_vault(&[("Orphan.md", "# Orphan\n\noriginal\n")]);
        let (_copy_dir, copy) = make_vault(&[("Orphan.md", "# Orphan\n\ncopy\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&original, &db_path, "default", "r4-vault")
            .unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let run = |root: &Path, name: &str| {
            let watcher = BrainWatcher::new(&db_path, root, "default", name);
            let stop = watcher.shutdown_handle();
            watcher
                .with_ready_callback(move || stop.stop())
                .run_with_store(store.clone(), None)
        };

        let message = format!("{:#}", run(&copy, "r4-vault").unwrap_err());
        assert!(
            message.contains("r4-vault")
                && message.contains(&*original.to_string_lossy())
                && message.contains("brain remove"),
            "{message}"
        );
        assert_eq!(store.list_vaults(None).unwrap().len(), 1);

        // Counterweight: re-watching the SAME root under its name still
        // works, and a first watch under a unique name still indexes.
        run(&original, "r4-vault").unwrap();
        run(&copy, "r4-copy").unwrap();
        let copy_uid = vault_uid("default", &copy.to_string_lossy());
        let copied = store.list_notes(Some(&copy_uid)).unwrap();
        assert_eq!(copied.len(), 1, "{copied:?}");
        assert_eq!(store.list_vaults(None).unwrap().len(), 2);
    }

    /// nw-653: notes created, edited or deleted while no watcher was
    /// processing events (daemon down, watcher wedged behind a deadlocked
    /// write gate, launchd crash-looping on a version mismatch) produced
    /// events nobody received. The next watcher only reacted to NEW events,
    /// so a created note stayed out of the graph until a full refresh — in
    /// the live repro, for days. Startup must reconcile disk against graph.
    #[test]
    fn watcher_startup_reconciles_notes_changed_while_no_watcher_ran() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n\nold body\n"),
            ("Gone.md", "# Gone\n\nwill be deleted\n"),
            ("Untouched.md", "# Untouched\n\n[[Alpha]]\n"),
            (".brainignore", "Ignored.md\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let notes = || -> HashMap<String, nestweaver_schema::Note> {
            store
                .list_notes(Some(&v_uid))
                .unwrap()
                .into_iter()
                .map(|note| (note.file_path.clone(), note))
                .collect()
        };
        let before = notes();

        // The gap: nothing is watching while these land on disk.
        fs::write(root.join("New.md"), "# New\n\ncreated in the gap\n").unwrap();
        fs::write(root.join("Ignored.md"), "# Ignored\n\nstays out\n").unwrap();
        // Over the default note limit: skipped and disclosed by any index,
        // so replaying it would publish nothing.
        let huge = format!(
            "# Huge\n\n{}",
            "x".repeat(crate::index_limits::DEFAULT_MAX_NOTE_BYTES as usize)
        );
        fs::write(root.join("Huge.md"), huge).unwrap();
        fs::write(root.join("Alpha.md"), "# Alpha\n\nedited in the gap\n").unwrap();
        // Recorded note mtimes are whole seconds; make the edit observable
        // even when the test runs inside the index's second.
        fs::OpenOptions::new()
            .write(true)
            .open(root.join("Alpha.md"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        fs::remove_file(root.join("Gone.md")).unwrap();

        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        // Exactly the drifted paths: never the untouched note (no re-parse),
        // the brainignored one, or the oversized one.
        let drift = crate::index_md::vault_startup_drift(
            &store,
            &root,
            "default",
            &watcher.ignore_set,
            watcher.note_limits,
        )
        .unwrap();
        assert_eq!(
            drift,
            vec![
                root.join("Alpha.md"),
                root.join("Gone.md"),
                root.join("New.md")
            ]
        );
        let stop = watcher.shutdown_handle();
        watcher
            .with_ready_callback(move || stop.stop())
            .run_with_store(store.clone(), None)
            .unwrap();

        let after = notes();
        assert!(
            after.contains_key("New.md"),
            "a note created while no watcher ran must be ingested at startup: {:?}",
            after.keys().collect::<Vec<_>>()
        );
        assert_ne!(
            after["Alpha.md"].content_hash, before["Alpha.md"].content_hash,
            "an edit made while no watcher ran must be ingested at startup"
        );
        assert!(
            !after.contains_key("Gone.md"),
            "a note deleted while no watcher ran must leave the graph"
        );
        assert!(
            !after.contains_key("Ignored.md"),
            "counterweight: a brainignored note stays out"
        );
        assert_eq!(
            after["Untouched.md"].content_hash,
            before["Untouched.md"].content_hash
        );

        // Counterweight: once reconciled, the next startup finds no drift and
        // runs no batch at all — unchanged notes are not re-parsed per start.
        let restarted = BrainWatcher::new(&db_path, &root, "default", "test");
        assert_eq!(
            restarted
                .reconcile_startup_drift(&store, None, &v_uid, &None)
                .unwrap(),
            0
        );
        assert!(restarted.last_batch_phase_timings().is_none());
    }

    /// Start a watcher over `root`, stop it at readiness, and return how many
    /// batch-scoped leases (`watch_vault_batch`) its startup acquired — zero
    /// means startup ran no batch at all.
    fn startup_batch_leases(db_path: &Path, root: &Path, store: &Arc<GraphStore>) -> u32 {
        use std::sync::atomic::AtomicU32;
        let batches = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&batches);
        let factory: WatchMutationLeaseFactory = Arc::new(move |label: &'static str| {
            if label == "watch_vault_batch" {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            Ok(Box::new(()) as Box<dyn WatchMutationLease>)
        });
        let watcher = BrainWatcher::new(db_path, root, "default", "test")
            .with_mutation_lease_factory(factory);
        let stop = watcher.shutdown_handle();
        watcher
            .with_ready_callback(move || stop.stop())
            .run_with_store(store.clone(), None)
            .unwrap();
        batches.load(Ordering::SeqCst)
    }

    /// nw-653 review: a note no index can ingest (here binary; a read or parse
    /// failure is the same class) is absent from the graph by design. Startup
    /// reconciliation must not treat it as a lost create and run a full batch
    /// and publication for it on every restart. It is retried once the file
    /// changes.
    #[test]
    fn watcher_startup_does_not_replay_a_note_that_cannot_be_ingested() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n"),
            ("Indexed.md", "# Indexed\n\u{0}binary\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let has = |path: &str| {
            store
                .list_notes(Some(&v_uid))
                .unwrap()
                .iter()
                .any(|note| note.file_path == path)
        };

        // Skipped by the full index: nothing to replay on the first start.
        assert_eq!(startup_batch_leases(&db_path, &root, &store), 0);

        // Created while no watcher ran: the first start must try it, and
        // once that attempt fails the next start must not try again.
        fs::write(root.join("Later.md"), "# Later\n\u{0}binary\n").unwrap();
        assert!(startup_batch_leases(&db_path, &root, &store) > 0);
        assert!(!has("Later.md"));
        assert_eq!(
            startup_batch_leases(&db_path, &root, &store),
            0,
            "an unchanged note that could not be ingested must not be replayed per start"
        );
        let disclosed = crate::index_md::load_skipped_notes_sidecar(&db_path);
        assert!(
            disclosed.skipped.iter().any(|file| file.path == "Later.md"),
            "the failed ingest stays disclosed: {:?}",
            disclosed.skipped
        );

        // Counterweight: fixed while no watcher ran, it is replayed and lands.
        fs::write(root.join("Later.md"), "# Later\n\nfixed\n").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(root.join("Later.md"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        assert!(startup_batch_leases(&db_path, &root, &store) > 0);
        assert!(has("Later.md"));
    }

    /// nw-653 review: a failed startup replay must not be dropped after a log
    /// line. The watcher keeps watching, discloses the debt in the
    /// skipped-notes status, retries, and clears the disclosure on success.
    #[test]
    fn failed_startup_reconciliation_is_disclosed_and_retried_until_it_lands() {
        use std::sync::atomic::AtomicBool;
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("Alpha.md", "# Alpha\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        fs::write(root.join("New.md"), "# New\n\ncreated in the gap\n").unwrap();

        // The first replay fails at its first batch lease; later ones succeed.
        let failed_once = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&failed_once);
        let factory: WatchMutationLeaseFactory = Arc::new(move |label: &'static str| {
            if label == "watch_vault_batch" && !flag.swap(true, Ordering::SeqCst) {
                anyhow::bail!("injected replay failure");
            }
            Ok(Box::new(()) as Box<dyn WatchMutationLease>)
        });
        let pending = |db: &Path| {
            crate::index_md::load_skipped_notes_sidecar(db)
                .skipped
                .into_iter()
                .filter(|file| {
                    file.reason
                        .starts_with(crate::index_md::WATCH_RECONCILIATION_PENDING_REASON)
                })
                .map(|file| file.path)
                .collect::<Vec<_>>()
        };

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_mutation_lease_factory(factory)
            .with_reconcile_retry_base(Duration::from_millis(300))
            .with_ready_callback(move || {
                let _ = ready_tx.send(());
            });
        let stop = watcher.shutdown_handle();
        let running = store.clone();
        let handle = thread::spawn(move || watcher.run_with_store(running, None));

        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("a failed reconciliation must not prevent readiness");
        assert!(
            failed_once.load(Ordering::SeqCst),
            "the injected failure ran"
        );
        assert_eq!(
            pending(&db_path),
            vec![root.join("New.md").to_string_lossy().into_owned()],
            "the owed reconciliation must be disclosed in status"
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
            thread::sleep(Duration::from_millis(50));
        }
        stop.stop();
        handle.join().unwrap().unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        assert!(
            store
                .list_notes(Some(&v_uid))
                .unwrap()
                .iter()
                .any(|note| note.file_path == "New.md"),
            "the retry must ingest the note"
        );
    }

    /// nw-653 review: a debt disclosed when the drift could not be computed
    /// must clear once a retry finds nothing left to reconcile; only the
    /// watcher clears it, so otherwise status reports it pending forever.
    #[test]
    fn a_retry_with_no_drift_clears_an_uncomputable_drift_debt() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("Alpha.md", "# Alpha\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        crate::index_md::record_vault_reconciliation_debt(
            Some(&db_path),
            &root,
            std::slice::from_ref(&root),
            Some("drift could not be computed"),
        );
        let pending = || {
            crate::index_md::skipped_notes_status_json(Some(&db_path)).0["reconciliation_pending"]
                .clone()
        };
        assert_eq!(pending(), serde_json::json!(1));

        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let next = watcher
            .attempt_reconciliation(&store, None, &v_uid, &None, None, Vec::new(), 1)
            .unwrap();
        assert!(next.is_none(), "nothing is left to reconcile");
        assert_eq!(pending(), serde_json::json!(0), "the debt must clear");
    }

    /// nw-651 on Linux (PR #431 CI): notify's inotify backend fails the
    /// WHOLE recursive watch on the first subdirectory it cannot watch, so a
    /// vault holding one unreadable directory had no brain watcher at all.
    /// The seam fails the subscription the way inotify does (FSEvents never
    /// would). Startup must proceed, the directory must be disclosed as the
    /// walk's unreadable-directory row and its notes kept, and edits
    /// elsewhere — including in a directory created afterwards — must land.
    #[cfg(unix)]
    #[test]
    fn brain_watcher_stays_live_when_a_subdirectory_cannot_be_watched() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n"),
            ("docs/Guide.md", "# Guide\n"),
            ("locked/Hidden.md", "# Hidden\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let locked = root.join("locked");
        let ignored = root.join("node_modules");
        fs::create_dir_all(&ignored).unwrap();
        let fresh_locked = root.join("fresh_locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(&ignored, fs::Permissions::from_mode(0o000)).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let has = |path: &str| {
            store
                .list_notes(Some(&v_uid))
                .unwrap()
                .iter()
                .any(|note| note.file_path == path)
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            // On Linux this is REAL inotify failing on the unreadable
            // directory; elsewhere the seam fails the way it does.
            let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
            #[cfg(not(target_os = "linux"))]
            let watcher = watcher.emulating_inotify_watch();
            let watcher = watcher.with_debounce_ms(100).with_ready_callback(move || {
                let _ = ready_tx.send(());
            });
            let stop = watcher.shutdown_handle();
            let running = store.clone();
            let handle = thread::spawn(move || watcher.run_with_store(running, None));
            let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let deadline = Instant::now() + Duration::from_secs(30);
                while ready_rx.try_recv().is_err() {
                    assert!(
                        !handle.is_finished(),
                        "an unwatchable subdirectory must not stop the watcher starting"
                    );
                    assert!(Instant::now() < deadline, "watcher never became ready");
                    thread::sleep(Duration::from_millis(50));
                }
                thread::sleep(Duration::from_millis(300));
                let rows = crate::index_md::load_skipped_notes_sidecar(&db_path).skipped;
                assert!(
                    rows.iter().any(|row| row.path == "locked"
                        && row.reason_code == nestweaver_parser::SkipReasonCode::ReadError),
                    "the unwatchable directory must be disclosed: {rows:?}"
                );
                assert!(
                    !rows.iter().any(|row| row.path == "node_modules"),
                    "an ignored unreadable directory is not disclosed: {rows:?}"
                );

                let wait = |what: &str, path: &str| {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while !has(path) {
                        assert!(Instant::now() < deadline, "timed out waiting for {what}");
                        thread::sleep(Duration::from_millis(50));
                    }
                };
                fs::write(root.join("docs/Fresh.md"), "# Fresh\n").unwrap();
                wait("a note in a watched directory", "docs/Fresh.md");
                fs::create_dir(root.join("newdir")).unwrap();
                fs::write(root.join("newdir/Born.md"), "# Born\n").unwrap();
                wait("a note in a directory created later", "newdir/Born.md");
                // Its own watch, not only the create event, carries later notes.
                thread::sleep(Duration::from_millis(300));
                fs::write(root.join("newdir/Later.md"), "# Later\n").unwrap();
                wait("a later note in the new directory", "newdir/Later.md");

                // Deleted and re-created within one debounce window: the new
                // directory needs a watch of its own again.
                fs::remove_dir_all(root.join("newdir")).unwrap();
                fs::create_dir(root.join("newdir")).unwrap();
                fs::write(root.join("newdir/Again.md"), "# Again\n").unwrap();
                wait("a note in the re-created directory", "newdir/Again.md");
                thread::sleep(Duration::from_millis(300));
                fs::write(root.join("newdir/After.md"), "# After\n").unwrap();
                wait(
                    "a later note in the re-created directory",
                    "newdir/After.md",
                );

                // Discovered live: disclosed when it appears unreadable.
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .mode(0o000)
                    .create(&fresh_locked)
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                while !crate::index_md::load_skipped_notes_sidecar(&db_path)
                    .skipped
                    .iter()
                    .any(|row| row.path == "fresh_locked")
                {
                    assert!(
                        Instant::now() < deadline,
                        "a directory found unreadable live must be disclosed"
                    );
                    thread::sleep(Duration::from_millis(50));
                }
                assert!(
                    has("locked/Hidden.md"),
                    "an unwatchable directory's notes are never deleted"
                );
            }));
            stop.stop();
            let joined = handle.join().unwrap();
            if let Err(panic) = checked {
                std::panic::resume_unwind(panic);
            }
            joined.unwrap();
        }));
        for dir in [&locked, &ignored, &fresh_locked] {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// nw-653 review: a partial scan is not a deletion. Notes under a
    /// subdirectory the walk cannot read are still in the graph and must not
    /// be replayed as deletions just because they were not enumerated.
    #[cfg(unix)]
    #[test]
    fn watcher_startup_drift_keeps_notes_in_an_unreadable_subdirectory() {
        use std::os::unix::fs::PermissionsExt;
        // Root reads through 0o000, so the fixture cannot lock it out (the
        // same guard as content_reader.rs's permission tests).
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n"),
            ("locked/Hidden.md", "# Hidden\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let drift = crate::index_md::vault_startup_drift(
            &store,
            &root,
            "default",
            &GlobSet::empty(),
            crate::index_limits::NoteLimits::default(),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let drift = drift.unwrap();
        assert!(
            !drift.contains(&locked.join("Hidden.md")),
            "an unreadable subdirectory must not be replayed as a deletion: {drift:?}"
        );
        assert!(drift.is_empty(), "{drift:?}");
    }

    /// nw-653 / nw-287 counterweight: an indexed vault whose scan finds no
    /// notes (unmounted, unreadable) is not evidence that every note was
    /// deleted, so startup reconciliation must not replay them as deletions.
    #[test]
    fn watcher_startup_drift_never_infers_deletions_from_an_empty_scan() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("Alpha.md", "# Alpha\n"), ("Beta.md", "# Beta\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        fs::remove_file(root.join("Alpha.md")).unwrap();
        fs::remove_file(root.join("Beta.md")).unwrap();
        let drift = crate::index_md::vault_startup_drift(
            &store,
            &root,
            "default",
            &GlobSet::empty(),
            crate::index_limits::NoteLimits::default(),
        )
        .unwrap();
        assert!(drift.is_empty(), "{drift:?}");
    }

    #[test]
    fn watcher_startup_preserves_untouched_vault_relationships() {
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n\n[[Beta]]\n"),
            ("Beta.md", "# Beta\n\n[[Gamma]]\n"),
            ("Gamma.md", "# Gamma\n\nUntouched content.\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let edges = || {
            let mut edges = store.load_vault_typed_edges(false).unwrap();
            edges.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
            edges
        };
        let before = edges();
        assert_eq!(
            before
                .iter()
                .filter(|edge| edge.2 == "VAULT_HAS_NOTE")
                .count(),
            3
        );
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let stop = watcher.shutdown_handle();
        watcher
            .with_ready_callback(move || stop.stop())
            .run_with_store(store.clone(), None)
            .unwrap();
        assert_eq!(
            edges(),
            before,
            "startup must preserve untouched graph relationships"
        );
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

    /// nw-475 (Task 5.2): the batch-complete event previously reported only
    /// `files`/`elapsed_ms`/`mutation_batch` — enough to see a batch was
    /// slow, never which of its phases was. Pins that every named phase is
    /// measured and carried on the SAME `BrainWatcher batch complete` event
    /// (via [`BatchPhaseTimings`]), mirroring how daemon boot broke
    /// `boot_ms` down into `store_open_ms`/`extension_reconcile_ms`/etc.
    /// (server.rs, nw-119) instead of leaving the reader to guess or
    /// instrument later. Asserted against the struct `process_batch` records
    /// for tests (`last_batch_phase_timings`) rather than by re-parsing log
    /// output, so this test needs no tracing-capture dependency.
    #[test]
    fn watcher_batch_logs_per_phase_timings() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("Alpha.md", "# Alpha\n\nold\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        fs::write(root.join("Alpha.md"), "# Alpha\n\nnew\n").unwrap();

        let batch_started = Instant::now();
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Alpha.md")], &None)
            .unwrap();
        let elapsed_ms = batch_started.elapsed().as_millis() as u64;

        let timings = watcher
            .last_batch_phase_timings()
            .expect("a graph batch must record phase timings");
        let accounted_ms = timings.build_symbol_index_ms
            + timings.embedding_candidates_ms
            + timings.refresh_watched_paths_ms
            + timings.sidecars_ms
            + timings.tombstones_ms
            + timings.non_graph_events_ms
            + timings.finalize_ms;
        assert!(
            accounted_ms <= elapsed_ms,
            "the named phases are non-overlapping sub-spans of the batch, \
             so their sum ({accounted_ms}ms) cannot exceed the batch's own \
             wall clock ({elapsed_ms}ms, measured independently by this test)"
        );
        // A zeroed struct would satisfy every assertion above (0 <= anything),
        // so that alone cannot prove the timers are wired up at all — a
        // real graph batch (parsing, sidecar writes, a database commit) must
        // account for SOME measurable time.
        assert!(
            accounted_ms > 0,
            "a real graph batch must account for measurable time somewhere, \
             not read as an all-zero struct: {timings:?}"
        );
    }

    /// COUNTERWEIGHT to `watcher_batch_logs_per_phase_timings`: a batch that
    /// touches no graph (markdown) path at all must take NONE of the
    /// graph-gated phases — proving those fields are actually wired to
    /// `graph_batch`, not placeholders that happen to read as zero for an
    /// unrelated reason.
    #[test]
    fn non_graph_only_batch_reports_zero_graph_phase_timings() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("Alpha.md", "# Alpha\n\nold\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        fs::write(root.join("note.txt"), "not markdown, so not a graph path").unwrap();

        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("note.txt")], &None)
            .unwrap();

        let timings = watcher
            .last_batch_phase_timings()
            .expect("even a non-graph batch records (zeroed) phase timings");
        assert_eq!(timings.embedding_candidates_ms, 0);
        assert_eq!(timings.refresh_watched_paths_ms, 0);
        assert_eq!(timings.sidecars_ms, 0);
        assert_eq!(timings.tombstones_ms, 0);
        assert_eq!(timings.finalize_ms, 0);
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
        // Restore the affected source so a later batch can succeed. Policy
        // skips for oversized notes are covered by
        // `watcher_discloses_note_grown_past_limit_without_failing_the_batch`.
        fs::write(root.join("Alpha.md"), "# Alpha\n\n[[Beta]]\n").unwrap();
    }

    /// nw-585 on the watcher's two routes: a live batch discloses a note
    /// whose frontmatter broke (and clears it once fixed), and startup
    /// reconciliation discloses one broken while no watcher ran.
    #[test]
    fn watcher_discloses_and_clears_unparsable_frontmatter() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[
            ("Live.md", "---\ntags: [a]\n---\n# Live\n"),
            ("Gap.md", "---\ntags: [b]\n---\n# Gap\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let unparsed = || -> Vec<String> {
            crate::index_md::skipped_notes_status_json(Some(&db_path)).0
                ["frontmatter_unparsed_notes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|row| row["path"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert!(unparsed().is_empty(), "precondition");

        // Live batch.
        fs::write(root.join("Live.md"), "---\ntags: [a\n---\n# Live\n").unwrap();
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Live.md")], &None)
            .unwrap();
        let rows = unparsed();
        assert!(
            rows.len() == 1 && rows[0].ends_with("Live.md"),
            "live batch: {rows:?}"
        );
        fs::write(root.join("Live.md"), "---\ntags: [a]\n---\n# Live\n").unwrap();
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Live.md")], &None)
            .unwrap();
        assert!(
            unparsed().is_empty(),
            "fixed in a live batch: {:?}",
            unparsed()
        );

        // Startup reconciliation: broken while nothing watched.
        fs::write(root.join("Gap.md"), "---\ntags: [b\n---\n# Gap\n").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(root.join("Gap.md"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        let restarted = BrainWatcher::new(&db_path, &root, "default", "test");
        let stop = restarted.shutdown_handle();
        restarted
            .with_ready_callback(move || stop.stop())
            .run_with_store(store.clone(), None)
            .unwrap();
        let rows = unparsed();
        assert!(
            rows.len() == 1 && rows[0].ends_with("Gap.md"),
            "startup reconciliation: {rows:?}"
        );
    }

    #[test]
    fn watcher_records_new_oversized_note_as_skipped() {
        let (_dir, root) = make_vault(&[("keep.md", "# Keep\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let huge = root.join("huge.md");
        fs::write(
            &huge,
            vec![b'x'; crate::index_md::MAX_NOTE_SIZE_BYTES as usize + 1],
        )
        .unwrap();
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        watcher
            .process_batch(&store, None, &v_uid, vec![huge], &None)
            .unwrap();
        let sidecar = crate::index_md::load_skipped_notes_sidecar(&db_path);
        assert!(
            sidecar.skipped.iter().any(|file| file.path == "huge.md"),
            "new oversized note must land in the sidecar: {sidecar:?}"
        );
        let notes = store.list_notes(Some(&v_uid)).unwrap();
        assert!(
            notes.iter().all(|note| note.file_path != "huge.md"),
            "oversized note must not be indexed: {notes:?}"
        );
        assert!(
            !crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "publication marker must stay clean after a disclosed skip"
        );
    }

    #[test]
    fn watcher_discloses_note_grown_past_limit_without_failing_the_batch() {
        let (_dir, root) = make_vault(&[("Beta.md", "# Beta\n\nold\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let beta = note_uid(&v_uid, "Beta.md");
        let before = store.lookup_note(&beta).unwrap().content_hash;
        fs::write(
            root.join("Beta.md"),
            vec![b'x'; crate::index_md::MAX_NOTE_SIZE_BYTES as usize + 1],
        )
        .unwrap();
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Beta.md")], &None)
            .unwrap();
        assert_eq!(store.lookup_note(&beta).unwrap().content_hash, before);
        let sidecar = crate::index_md::load_skipped_notes_sidecar(&db_path);
        assert!(
            sidecar.skipped.iter().any(|file| file.path == "Beta.md"),
            "grown-past-limit note must be disclosed: {sidecar:?}"
        );
        assert!(
            !crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "publication marker must stay clean"
        );
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
            .establish_graph_publication_with_io(
                &store,
                &crate::index::FileSystemIndexEpilogueIo,
                &[],
            )
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
    fn canonical_manifest_watch_queues_recovery_without_trusting_legacy_sidecar() {
        let (_dir, root) = make_vault(&[(
            "package.json",
            r#"{"name":"watched-package","dependencies":{"watched-dep":"1"}}"#,
        )]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        insert_watched_repo(&store, &root, "repo:watched");
        let legacy_path = db_path.with_extension("manifests.json");
        crate::save_manifest_cache(&HashMap::new(), &legacy_path).unwrap();
        let canonical_path = crate::manifest_cache_path(&db_path);
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(&canonical_path);

        watcher
            .handle_non_graph_event(&store, root.join("package.json"))
            .unwrap();

        assert!(!canonical_path.exists());
        assert!(
            legacy_path.exists(),
            "legacy evidence stays until daemon publication"
        );
        assert!(
            crate::manifest::manifest_debt_revision(&db_path)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            manifest_cache_repo_uid(&store, &root).as_deref(),
            Some("repo:watched")
        );
    }

    /// nw-498. The MCP response cache keys every hit on `graph_generation`
    /// (and the filemeta scope digest). A `package.json` is not a parsed
    /// source file, so it appears in no filemeta slice, and the watcher's
    /// manifest refresh keyed by path and advanced nothing — so a cached
    /// `dead_code`, whose entry points come from exactly this sidecar, kept
    /// being served from the pre-edit answer. Adding or removing a package
    /// entry point changes which symbols are reachable, and the stale side of
    /// that is a LIVE symbol still listed as dead.
    ///
    /// The generation is asserted both in memory and in the `<db>.generation`
    /// sidecar: a short-lived CLI process loads the counter from that file on
    /// open, so a bump that is not persisted is invisible to exactly the
    /// callers the cache exists for.
    #[test]
    fn a_manifest_edit_advances_and_persists_the_graph_generation() {
        let (_dir, root) = make_vault(&[(
            "package.json",
            r#"{"name":"watched-package","main":"entry.js"}"#,
        )]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        insert_watched_repo(&store, &root, "repo:watched");
        let canonical_path = crate::manifest_cache_path(&db_path);
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(&canonical_path);

        let before = store.graph_generation();
        watcher
            .handle_non_graph_event(&store, root.join("package.json"))
            .unwrap();
        let after = store.graph_generation();

        assert!(
            after > before,
            "a manifest edit must advance the graph generation, or every \
             response cached against {before} outlives the edit that \
             invalidated it (got {after})"
        );
        let persisted = std::fs::read_to_string(&generation_path)
            .expect("the advance must be persisted, or a fresh process never observes it")
            .trim()
            .parse::<u64>()
            .expect("the generation sidecar holds a decimal integer");
        assert_eq!(
            persisted, after,
            "the persisted generation must match the live one"
        );

        assert!(
            crate::manifest::manifest_debt_revision(&db_path)
                .unwrap()
                .is_some()
        );
        assert!(
            !canonical_path.exists(),
            "watcher queues a complete derivation, not a singleton"
        );
    }

    /// nw-522. Path keys were purged as non-live by
    /// `reconcile_deleted_graph_state` because that retain is a UID set.
    /// After a UID-keyed write, reconciliation must keep the entry.
    #[test]
    fn pending_manifest_debt_survives_deleted_graph_reconciliation() {
        let (_dir, root) = make_vault(&[(
            "package.json",
            r#"{"name":"watched-package","main":"entry.js"}"#,
        )]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        insert_watched_repo(&store, &root, "repo:watched");
        let canonical_path = crate::manifest_cache_path(&db_path);
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(&canonical_path);

        watcher
            .handle_non_graph_event(&store, root.join("package.json"))
            .unwrap();
        crate::reconcile_deleted_graph_state(&store, &db_path);
        assert!(
            crate::manifest::manifest_debt_revision(&db_path)
                .unwrap()
                .is_some()
        );
        assert!(crate::manifest::current_manifest_snapshot(&store, &db_path).is_err());
    }

    /// nw-522's counterweight. A path key accidentally survived a working-tree
    /// move because the new path was a new map entry. UID lookup follows
    /// `root_path`, so after the graph records the move the same UID is
    /// refreshed rather than a second (path) key appearing.
    #[test]
    fn a_moved_repo_queues_manifest_recovery_under_its_uid() {
        let (_dir, root) = make_vault(&[(
            "package.json",
            r#"{"name":"watched-package","main":"old.js"}"#,
        )]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        insert_watched_repo(&store, &root, "repo:watched");
        let canonical_path = crate::manifest_cache_path(&db_path);
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(&canonical_path);

        watcher
            .handle_non_graph_event(&store, root.join("package.json"))
            .unwrap();

        let moved = db_dir.path().join("moved-tree");
        fs::create_dir_all(&moved).unwrap();
        fs::write(
            moved.join("package.json"),
            r#"{"name":"moved-package","main":"new.js"}"#,
        )
        .unwrap();
        let moved = fs::canonicalize(&moved).unwrap();
        store
            .update_repo_root_path("repo:watched", &moved.to_string_lossy())
            .unwrap();

        watcher
            .handle_non_graph_event(&store, moved.join("package.json"))
            .unwrap();
        assert_eq!(
            manifest_cache_repo_uid(&store, &moved).as_deref(),
            Some("repo:watched")
        );
        assert_eq!(manifest_cache_repo_uid(&store, &root), None);
        assert!(
            crate::manifest::manifest_debt_revision(&db_path)
                .unwrap()
                .is_some()
        );
        assert!(!canonical_path.exists());
    }

    #[test]
    fn a_manifest_edit_without_an_indexed_repo_does_not_write_a_path_key() {
        let (_dir, root) = make_vault(&[("package.json", r#"{"name":"orphan"}"#)]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let canonical_path = crate::manifest_cache_path(&db_path);
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(&canonical_path);

        let outcome = watcher
            .handle_non_graph_event(&store, root.join("package.json"))
            .unwrap();
        match outcome {
            UpdateOutcome::Skipped { reason, .. } => {
                assert_eq!(reason, "manifest file — no indexed repo");
            }
            other => panic!("expected skip, got {other:?}"),
        }
        assert!(
            !canonical_path.exists(),
            "no indexed repo means no cache write"
        );
    }

    #[test]
    fn deleted_nested_manifest_is_admitted_and_queues_complete_recovery() {
        let (_dir, root) = make_vault(&[("packages/app/package.json", r#"{"name":"app"}"#)]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        insert_watched_repo(&store, &root, "repo:watched");
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(crate::manifest_cache_path(&db_path));
        let path = root.join("packages/app/package.json");
        fs::remove_file(&path).unwrap();
        assert!(watcher.event_targets_manifest(&path));
        let outcome = watcher.handle_non_graph_event(&store, path).unwrap();
        assert!(matches!(
            outcome,
            UpdateOutcome::Skipped {
                reason: "manifest file — reconciliation pending",
                ..
            }
        ));
        assert!(
            crate::manifest::manifest_debt_revision(&db_path)
                .unwrap()
                .is_some()
        );
    }

    /// nw-498's counterweight. The invalidation is scoped to MANIFEST edits.
    /// An ordinary note edit — the overwhelmingly common watcher event, many
    /// per minute in a live vault — must not advance the generation, or the
    /// response cache is effectively disabled for anyone running
    /// `brain watch` and every dependent read recomputes from scratch.
    #[test]
    fn an_unrelated_note_edit_does_not_advance_the_graph_generation() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[
            ("package.json", r#"{"name":"watched-package"}"#),
            ("note.md", "# Note\n\nBody.\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let canonical_path = crate::manifest_cache_path(&db_path);
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(&canonical_path);

        let before = store.graph_generation();
        watcher
            .handle_non_graph_event(&store, root.join("note.md"))
            .unwrap();

        assert_eq!(
            store.graph_generation(),
            before,
            "a note edit is not a manifest edit and must leave the generation \
             alone; bumping on every watched file would invalidate every \
             cached response in a live vault"
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
        // nw-652: a `target/` notes folder with no build manifest beside it is
        // an ordinary folder name for notes, and is watched.
        let p = Path::new("/x/vault/target/Range Day.md");
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

    // ── nw-668: the sidecar phase is set-based, transactional, off-lease ──

    fn nw_668_insert_symbol(store: &GraphStore, repo: &str, name: &str, line: u32) {
        store
            .insert_symbol(&nestweaver_schema::Symbol {
                uid: nestweaver_schema::symbol_uid(repo, "src/lib.rs", name, line),
                name: name.to_string(),
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid: repo.to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: line,
                end_line: line,
                signature: format!("fn {name}()"),
                summary: None,
                content_hash: "h".to_string(),
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
    }

    struct Nw668Fixture {
        _vault_dir: tempfile::TempDir,
        root: PathBuf,
        _db_dir: tempfile::TempDir,
        db_path: PathBuf,
        store: GraphStore,
        v_uid: String,
    }

    /// A vault of `notes` indexed notes plus a repo of `symbols` symbols.
    fn nw_668_fixture(files: &[(&str, &str)], notes: usize, symbols: &[&str]) -> Nw668Fixture {
        let mut all: Vec<(String, String)> = (0..notes)
            .map(|i| (format!("n{i:03}.md"), format!("# N{i}\n\nplain body {i}\n")))
            .collect();
        all.extend(files.iter().map(|(p, c)| (p.to_string(), c.to_string())));
        let borrowed: Vec<(&str, &str)> =
            all.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        let (vault_dir, root) = make_vault(&borrowed);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        insert_watched_repo(&store, &root, "repo:nw668");
        for (line, name) in symbols.iter().enumerate() {
            nw_668_insert_symbol(&store, "repo:nw668", name, line as u32 + 1);
        }
        let v_uid = vault_uid("default", &root.to_string_lossy());
        Nw668Fixture {
            _vault_dir: vault_dir,
            root,
            _db_dir: db_dir,
            db_path,
            store,
            v_uid,
        }
    }

    /// nw-670 R5: put every note of the fixture in one project over `repos`,
    /// so a name defined in more than one file can still link (an unscoped
    /// note links only single-definition names).
    fn nw_670_project_over(fx: &Nw668Fixture, repos: &[&str]) {
        let project = nestweaver_schema::Project {
            uid: "proj:nw670".to_string(),
            name: "nw670".to_string(),
            summary: None,
            instance_id: "default".to_string(),
        };
        let notes: Vec<(String, String)> = fx
            .store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .map(|note| (project.uid.clone(), note.uid))
            .collect();
        let symbols: Vec<(String, String)> = fx
            .store
            .list_symbols_for_linking()
            .unwrap()
            .into_iter()
            .filter(|(_, _, _, repo, _)| repos.contains(&repo.as_str()))
            .map(|(uid, _, _, _, _)| (project.uid.clone(), uid))
            .collect();
        fx.store
            .replace_materialized_projects(&[project], &notes, &symbols, &[], &[])
            .unwrap();
    }

    /// Mentions every name in the whole note and once more in a second
    /// section, so each edited note yields `k` note edges and `k + 1`
    /// section edges.
    fn nw_668_edited_body(title: &str, names: &[String]) -> String {
        format!(
            "# {title}\n\n{}\n\n## Tail\n\n{}\n",
            names.join(" "),
            names[0]
        )
    }

    fn nw_668_statements_for(k: usize) -> (BatchPhaseTimings, usize) {
        let names: Vec<String> = (0..k).map(|j| format!("Widget{j:04}Handler")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let fx = nw_668_fixture(&[], 200, &name_refs);
        for i in 0..2 {
            fs::write(
                fx.root.join(format!("n{i:03}.md")),
                nw_668_edited_body(&format!("Edited {i}"), &names),
            )
            .unwrap();
        }
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        watcher
            .process_batch(
                &fx.store,
                None,
                &fx.v_uid,
                vec![fx.root.join("n000.md"), fx.root.join("n001.md")],
                &None,
            )
            .unwrap();
        let edges = fx.store.list_references_code_edges().unwrap().len();
        (watcher.last_batch_phase_timings().unwrap(), edges)
    }

    /// nw-668: the watcher wrote every note→symbol and section→symbol edge
    /// as its own auto-committed statement, so one edited note with 10K–100K
    /// name matches cost as many commits and held the write lease for up to
    /// 22 minutes. The flush must be set-based: the statement and
    /// transaction counts depend on how many NOTES a batch touches, never on
    /// how many symbols each note mentions.
    #[test]
    fn nw_668_sidecar_flush_statements_do_not_scale_with_matches_per_note() {
        let _guard = serial_watcher_test();
        let (small, small_edges) = nw_668_statements_for(10);
        let (large, large_edges) = nw_668_statements_for(500);

        // Counterweight: the fixture really does produce K-proportional
        // edges, so equal statement counts below are not two empty flushes.
        assert_eq!(small_edges, 2 * (10 + 10 + 1));
        assert_eq!(large_edges, 2 * (500 + 500 + 1));

        assert!(
            small.sidecar_transactions <= 2usize.div_ceil(crate::cross_domain::NOTES_PER_TXN),
            "a 2-note batch flushes in ONE transaction: {small:?}"
        );
        assert_eq!(small.sidecar_transactions, large.sidecar_transactions);
        assert_eq!(
            small.sidecar_statements, large.sidecar_statements,
            "statements must not scale with matches per note: K=10 {small:?} vs K=500 {large:?}"
        );
        assert!(
            large.sidecar_statements <= 4,
            "one delete per edge kind and one insert per edge kind: {large:?}"
        );
        assert_eq!(large.sidecar_edges, 2 * (500 + 500 + 1));
        // The same property observed at the store's execute boundary, not
        // taken from the flush's report about itself.
        assert_eq!(
            small.sidecar_boundary_statements, large.sidecar_boundary_statements,
            "boundary-counted statements must not scale with matches per note"
        );
        assert!(large.sidecar_boundary_statements <= 4, "{large:?}");
        assert!(
            large.sidecar_boundary_statements > 0,
            "the counter must see the flush"
        );
    }

    /// nw-673: the configured `[cross_domain]` stoplist reaches the
    /// watcher's link scan. Counterweight: the other edited note links.
    #[test]
    fn nw_673_the_configured_stoplist_reaches_the_watcher() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget", "BravoWidget"]);
        fs::write(fx.root.join("n000.md"), "# A\n\nuses AlphaWidget\n").unwrap();
        fs::write(fx.root.join("n001.md"), "# B\n\nuses BravoWidget\n").unwrap();
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test")
            .with_cross_domain_config(crate::config::CrossDomainConfig {
                stoplist_extend: vec!["AlphaWidget".to_string()],
                ..Default::default()
            });
        watcher
            .process_batch(
                &fx.store,
                None,
                &fx.v_uid,
                vec![fx.root.join("n000.md"), fx.root.join("n001.md")],
                &None,
            )
            .unwrap();
        assert_eq!(
            fx.store.list_references_code_edges().unwrap().len(),
            2,
            "only n001's note + section edge"
        );
    }

    /// nw-668: the lease was taken once PER NOTE and held across the scan.
    /// The scan (tokenising bodies against a 200K-symbol index) is read-only
    /// and must run with no lease held; the flush takes one lease per
    /// NOTES_PER_TXN chunk, so a 2-note batch takes exactly one.
    #[test]
    fn nw_668_sidecars_scan_off_lease_and_take_one_lease_per_chunk() {
        use std::sync::atomic::AtomicI32;

        struct LiveLease(Arc<AtomicI32>);
        impl Drop for LiveLease {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 3, &["AlphaWidget", "BravoWidget"]);
        fs::write(fx.root.join("n000.md"), "# A\n\nuses AlphaWidget\n").unwrap();
        fs::write(fx.root.join("n001.md"), "# B\n\nuses BravoWidget\n").unwrap();

        let live = Arc::new(AtomicI32::new(0));
        let labels = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let (live_f, labels_f) = (Arc::clone(&live), Arc::clone(&labels));
        let factory: WatchMutationLeaseFactory = Arc::new(move |label: &'static str| {
            labels_f.lock().unwrap().push(label);
            live_f.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(LiveLease(Arc::clone(&live_f))) as Box<dyn WatchMutationLease>)
        });
        let observed = Arc::new(Mutex::new(Vec::<i32>::new()));
        let (observed_p, live_p) = (Arc::clone(&observed), Arc::clone(&live));
        let mut watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test")
            .with_mutation_lease_factory(factory);
        watcher.sidecar_scan_probe = Some(Box::new(move |_held| {
            observed_p
                .lock()
                .unwrap()
                .push(live_p.load(Ordering::SeqCst));
        }));

        watcher
            .process_batch(
                &fx.store,
                None,
                &fx.v_uid,
                vec![fx.root.join("n000.md"), fx.root.join("n001.md")],
                &None,
            )
            .unwrap();

        let sidecar_leases = labels
            .lock()
            .unwrap()
            .iter()
            .filter(|label| **label == "watch_vault_sidecars")
            .count();
        assert_eq!(sidecar_leases, 1, "labels: {:?}", labels.lock().unwrap());
        let observed = observed.lock().unwrap().clone();
        assert!(!observed.is_empty(), "the scan probe must have run");
        assert!(
            observed.iter().all(|live| *live == 0),
            "the cross-domain scan must run with NO lease held: {observed:?}"
        );
        // Counterweight: the edges were still written.
        assert_eq!(fx.store.list_references_code_edges().unwrap().len(), 4);
    }

    /// nw-668: moving the scan off the disk re-read (onto the text the batch
    /// committed) and onto set-based statements must not change WHICH edges
    /// exist. The fixture covers every shape the old whole-file scan saw:
    /// frontmatter, heading text, a preamble, a link inside a heading, a
    /// setext heading, a repeated mention, a stoplisted and a too-short name,
    /// and a trailing empty section. Pinned twice: against the literal set
    /// the pre-nw-668 path produced (characterised on that code), and
    /// against the bulk `discover_cross_domain_links` pass over the same
    /// files. The `@b` rows (one name defined in two repos of the notes'
    /// project links to both, nw-670 R5/R6) are pinned against the bulk
    /// pass. nw-670 re-pinned the literal set under its rules; the bulk
    /// comparison is the parity check that must never be re-pinned.
    #[test]
    fn nw_668_watcher_edges_equal_the_pre_change_and_bulk_edge_sets() {
        let _guard = serial_watcher_test();
        let symbols = [
            "FrontThing",
            "HeadThing",
            "PreambleThing",
            "LinkThing",
            "UrlThing",
            "SetextThing",
            "BodyThing",
            "TwiceThing",
            "Error",
            "Foo",
            "NeverMentioned",
        ];
        let fx = nw_668_fixture(&[], 2, &symbols);
        // The same name in a second repo of the notes' project: a mention
        // links to BOTH symbols, each at half the confidence (nw-670 R9).
        nw_668_insert_symbol(&fx.store, "repo:nw668b", "BodyThing", 1);
        nw_670_project_over(&fx, &["repo:nw668", "repo:nw668b"]);
        fs::write(
            fx.root.join("n000.md"),
            "---\nrelated: FrontThing\n---\nPreambleThing intro\n\n\
             # HeadThing title\n\nBodyThing TwiceThing TwiceThing Error Foo\n\n\
             ## See [LinkThing](https://example.com/UrlThing)\n\nTwiceThing again\n\n\
             SetextThing\n-----------\n\nplain\n\n## Empty\n",
        )
        .unwrap();
        fs::write(fx.root.join("n001.md"), "# Other\n\nBodyThing only\n").unwrap();
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        watcher
            .process_batch(
                &fx.store,
                None,
                &fx.v_uid,
                vec![fx.root.join("n000.md"), fx.root.join("n001.md")],
                &None,
            )
            .unwrap();

        // Render every edge as `<note-file>[:<section start line>] -> <symbol name>`.
        let render = |store: &GraphStore| -> Vec<String> {
            let mut labels: HashMap<String, String> = HashMap::new();
            for file in ["n000.md", "n001.md"] {
                let uid = note_uid(&fx.v_uid, file);
                for section in store.sections_in_note(&uid).unwrap() {
                    labels.insert(section.uid, format!("{file}:{}", section.start_line));
                }
                labels.insert(uid, file.to_string());
            }
            let mut out: Vec<String> = store
                .list_references_code_edges()
                .unwrap()
                .into_iter()
                .map(|(from, to, confidence, source)| {
                    let name = if to
                        == nestweaver_schema::symbol_uid(
                            "repo:nw668b",
                            "src/lib.rs",
                            "BodyThing",
                            1,
                        ) {
                        "BodyThing@b"
                    } else {
                        symbols
                            .iter()
                            .enumerate()
                            .find(|(line, name)| {
                                nestweaver_schema::symbol_uid(
                                    "repo:nw668",
                                    "src/lib.rs",
                                    name,
                                    *line as u32 + 1,
                                ) == to
                            })
                            .map(|(_, name)| *name)
                            .unwrap()
                    };
                    format!(
                        "{} -> {name} ({confidence:.1}, {source})",
                        labels.get(&from).cloned().unwrap_or(from)
                    )
                })
                .collect();
            out.sort();
            out
        };
        let watched = render(&fx.store);
        // Re-pinned for nw-670's rules. Against the pre-nw-668 set: UrlThing
        // is gone (a URL is never a mention, R1); every mention here is
        // distinctive prose, so confidence is 0.9 × 0.85 (R9, shown as 0.8);
        // BodyThing's two definitions split one unit of mass (0.4 each).
        // The rest — frontmatter, heading, preamble, link text, setext,
        // repeated and stoplisted/short names, the empty section — is as
        // before.
        let expected: Vec<String> = vec![
            "n000.md -> BodyThing (0.4, name-match)",
            "n000.md -> BodyThing@b (0.4, name-match)",
            "n000.md -> FrontThing (0.8, name-match)",
            "n000.md -> HeadThing (0.8, name-match)",
            "n000.md -> LinkThing (0.8, name-match)",
            "n000.md -> PreambleThing (0.8, name-match)",
            "n000.md -> SetextThing (0.8, name-match)",
            "n000.md -> TwiceThing (0.8, name-match)",
            "n000.md:11 -> TwiceThing (0.8, name-match)",
            "n000.md:4 -> PreambleThing (0.8, name-match)",
            "n000.md:7 -> BodyThing (0.4, name-match)",
            "n000.md:7 -> BodyThing@b (0.4, name-match)",
            "n000.md:7 -> TwiceThing (0.8, name-match)",
            "n001.md -> BodyThing (0.4, name-match)",
            "n001.md -> BodyThing@b (0.4, name-match)",
            "n001.md:2 -> BodyThing (0.4, name-match)",
            "n001.md:2 -> BodyThing@b (0.4, name-match)",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(
            watched, expected,
            "watcher edge set drifted from the pinned nw-670 set"
        );

        crate::cross_domain::discover_cross_domain_links(&fx.store).unwrap();
        assert_eq!(
            render(&fx.store),
            watched,
            "the watcher and the bulk pass must agree on the same files"
        );
    }

    /// nw-668: a batch that edits two notes and deletes a third paid one
    /// Tantivy commit (fsync + reader reload) per note under the lease. It
    /// must pay one for the whole batch — deletions included.
    #[test]
    fn nw_668_one_tantivy_commit_per_batch() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 3, &["AlphaWidget"]);
        let tantivy = TantivyIndex::open_or_create(&fx.db_path.with_extension("tantivy")).unwrap();
        tantivy.reindex_from_store(&fx.store).unwrap();
        let deleted = note_uid(&fx.v_uid, "n002.md");
        assert!(
            tantivy
                .search("N2", 10)
                .unwrap()
                .iter()
                .any(|hit| hit.uid == deleted || hit.note_uid == deleted),
            "precondition: the note to delete is searchable"
        );
        fs::write(fx.root.join("n000.md"), "# A\n\nuses AlphaWidget zebra\n").unwrap();
        fs::write(fx.root.join("n001.md"), "# B\n\nuses AlphaWidget yak\n").unwrap();
        fs::remove_file(fx.root.join("n002.md")).unwrap();
        let before = tantivy.commit_count();
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        watcher
            .process_batch(
                &fx.store,
                Some(&tantivy),
                &fx.v_uid,
                vec![
                    fx.root.join("n000.md"),
                    fx.root.join("n001.md"),
                    fx.root.join("n002.md"),
                ],
                &None,
            )
            .unwrap();
        assert_eq!(tantivy.commit_count() - before, 1);
        // Counterweight: the one commit carried all three changes.
        assert!(!tantivy.search("zebra", 10).unwrap().is_empty());
        assert!(!tantivy.search("yak", 10).unwrap().is_empty());
        assert!(
            tantivy
                .search("N2", 10)
                .unwrap()
                .iter()
                .all(|hit| hit.uid != deleted && hit.note_uid != deleted),
            "the deleted note's docs must be gone"
        );
    }

    fn nw_668_owed_debt(db_path: &Path) -> Vec<String> {
        crate::index_md::load_skipped_notes_sidecar(db_path)
            .skipped
            .into_iter()
            .filter(|file| {
                file.reason
                    .starts_with(crate::index_md::WATCH_RECONCILIATION_PENDING_REASON)
            })
            .map(|file| file.path)
            .collect()
    }

    fn nw_668_targets(store: &GraphStore) -> Vec<String> {
        store
            .list_references_code_edges()
            .unwrap()
            .into_iter()
            .map(|(_, to, _, _)| to)
            .collect()
    }

    /// nw-668 review (I1): the whole batch was scanned into memory before the
    /// first chunk flushed, so a startup replay of thousands of heavily
    /// linked notes held every one of their edge lists at once. Scanning and
    /// flushing interleave: at most one chunk of scanned notes is held while
    /// the next note is scanned, each chunk takes its own lease and
    /// transaction, and Tantivy still commits once for the batch.
    #[test]
    fn nw_668_a_large_batch_scans_and_flushes_one_chunk_at_a_time() {
        let _guard = serial_watcher_test();
        let notes = crate::cross_domain::NOTES_PER_TXN + 1;
        let fx = nw_668_fixture(&[], notes, &["AlphaWidget", "BodyThing"]);
        nw_668_insert_symbol(&fx.store, "repo:nw668b", "BodyThing", 1);
        nw_670_project_over(&fx, &["repo:nw668", "repo:nw668b"]);
        let tantivy = TantivyIndex::open_or_create(&fx.db_path.with_extension("tantivy")).unwrap();
        let mut paths = Vec::new();
        for i in 0..notes {
            let path = fx.root.join(format!("n{i:03}.md"));
            fs::write(
                &path,
                format!("# N{i}\n\nuses AlphaWidget\n\n## B\n\nBodyThing\n"),
            )
            .unwrap();
            paths.push(path);
        }

        let labels = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let labels_f = Arc::clone(&labels);
        let factory: WatchMutationLeaseFactory = Arc::new(move |label: &'static str| {
            labels_f.lock().unwrap().push(label);
            Ok(Box::new(()) as Box<dyn WatchMutationLease>)
        });
        let held = Arc::new(Mutex::new(Vec::<usize>::new()));
        let held_p = Arc::clone(&held);
        let mut watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test")
            .with_mutation_lease_factory(factory);
        watcher.sidecar_scan_probe = Some(Box::new(move |count| {
            held_p.lock().unwrap().push(count);
        }));
        let commits = tantivy.commit_count();
        let owed = watcher
            .process_batch(&fx.store, Some(&tantivy), &fx.v_uid, paths, &None)
            .unwrap();
        assert!(owed.is_none());

        let held = held.lock().unwrap().clone();
        assert_eq!(held.len(), notes, "every note was scanned");
        assert!(
            held.iter()
                .all(|count| *count < crate::cross_domain::NOTES_PER_TXN),
            "a full chunk must be flushed before the next note is scanned: max held {:?}",
            held.iter().max()
        );
        let sidecar_leases = labels
            .lock()
            .unwrap()
            .iter()
            .filter(|label| **label == "watch_vault_sidecars")
            .count();
        assert_eq!(sidecar_leases, 2);
        assert_eq!(
            watcher
                .last_batch_phase_timings()
                .unwrap()
                .sidecar_transactions,
            2
        );
        assert_eq!(tantivy.commit_count() - commits, 1);

        // Equivalence at chunk scale, including a name defined in two repos.
        let watched = fx.store.list_references_code_edges().unwrap();
        assert_eq!(
            watched.len(),
            notes * 6,
            "2 note + 1 section edge + 2 BodyThing each"
        );
        crate::cross_domain::discover_cross_domain_links(&fx.store).unwrap();
        assert_eq!(fx.store.list_references_code_edges().unwrap(), watched);
    }

    /// nw-668 review (I2): a flush that failed was only logged, and nothing
    /// ever rebuilt the notes' code links — the refresh had already replaced
    /// their nodes, so they sat with none. One immediate retry absorbs a
    /// transient failure inside the batch.
    #[test]
    fn nw_668_a_transient_flush_failure_is_retried_within_the_batch() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget"]);
        fs::write(fx.root.join("n000.md"), "# A\n\nuses AlphaWidget\n").unwrap();
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(1));
        let owed = watcher.process_batch(
            &fx.store,
            None,
            &fx.v_uid,
            vec![fx.root.join("n000.md")],
            &None,
        );
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(0));
        assert!(owed.unwrap().is_none(), "the retry landed; nothing is owed");
        assert_eq!(nw_668_targets(&fx.store).len(), 2);
        assert_eq!(
            watcher
                .last_batch_phase_timings()
                .unwrap()
                .sidecar_flush_failures,
            1
        );
    }

    /// nw-668 review (I2): when the immediate retry also fails, the chunk is
    /// all-or-nothing (no partial edge set), the batch still publishes the
    /// committed text, and the notes are OWED: disclosed in the skipped-notes
    /// status and replayed through the reconciliation retry, which rebuilds
    /// their links and clears the disclosure.
    #[test]
    fn nw_668_a_live_flush_failure_is_owed_disclosed_and_healed_by_the_retry() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget"]);
        let path = fx.root.join("n000.md");
        fs::write(&path, "# A\n\nuses AlphaWidget\n").unwrap();
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counted = Arc::clone(&calls);
        let callback: Option<Box<dyn Fn() + Send>> = Some(Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));

        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(2));
        let owed = watcher.process_batch(&fx.store, None, &fx.v_uid, vec![path.clone()], &callback);
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(0));
        let owed = owed.unwrap().expect("a twice-failed flush is owed");
        assert_eq!(owed.paths, vec![path.clone()]);
        assert_eq!(
            nw_668_targets(&fx.store),
            Vec::<String>::new(),
            "no partial set"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the committed text still publishes"
        );

        let pending = watcher.owe_sidecar_retry(&fx.store, None, owed);
        assert_eq!(pending.paths.as_deref(), Some(&[path.clone()][..]));
        assert_eq!(
            nw_668_owed_debt(&fx.db_path),
            vec![path.to_string_lossy().into_owned()],
            "the owed notes are disclosed"
        );
        let reasons: Vec<String> = crate::index_md::load_skipped_notes_sidecar(&fx.db_path)
            .skipped
            .into_iter()
            .map(|file| file.reason)
            .collect();
        let expected_prefix = format!(
            "{}: brain watcher code links not yet written; retrying (",
            crate::index_md::WATCH_RECONCILIATION_PENDING_REASON
        );
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(
            reasons[0].starts_with(&expected_prefix)
                && reasons[0].contains("injected cross-domain flush failure")
                && !reasons[0].contains("startup"),
            "a live link failure must say what is owed, not blame startup: {reasons:?}"
        );

        let next = watcher
            .attempt_reconciliation(
                &fx.store,
                None,
                &fx.v_uid,
                &None,
                pending.paths,
                pending.also_replay,
                pending.failures,
            )
            .unwrap();
        assert!(next.is_none(), "the retry landed");
        assert_eq!(nw_668_targets(&fx.store).len(), 2, "links rebuilt");
        assert!(
            nw_668_owed_debt(&fx.db_path).is_empty(),
            "disclosure cleared"
        );
    }

    /// nw-668 review (I2): the startup replay is the same seam. A replay
    /// whose sidecar flush fails twice is not "reconciled": it stays owed
    /// and disclosed, and the next attempt heals it.
    #[test]
    fn nw_668_a_startup_replay_with_a_failed_flush_stays_owed() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget"]);
        let path = fx.root.join("n000.md");
        fs::write(&path, "# A\n\nuses AlphaWidget\n").unwrap();
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");

        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(2));
        let first = watcher.attempt_reconciliation(
            &fx.store,
            None,
            &fx.v_uid,
            &None,
            Some(vec![path.clone()]),
            Vec::new(),
            0,
        );
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(0));
        let pending = first
            .unwrap()
            .expect("a replay whose links failed is still owed");
        assert_eq!(pending.paths.as_deref(), Some(&[path.clone()][..]));
        assert_eq!(pending.failures, 1);
        assert_eq!(
            nw_668_owed_debt(&fx.db_path),
            vec![path.to_string_lossy().into_owned()]
        );
        assert!(
            crate::index_md::load_skipped_notes_sidecar(&fx.db_path)
                .skipped
                .iter()
                .all(|file| file
                    .reason
                    .contains("brain watcher code links not yet written")),
            "a replay whose text landed discloses owed links, not a failed startup"
        );

        let next = watcher
            .attempt_reconciliation(
                &fx.store,
                None,
                &fx.v_uid,
                &None,
                pending.paths,
                pending.also_replay,
                pending.failures,
            )
            .unwrap();
        assert!(next.is_none());
        assert_eq!(nw_668_targets(&fx.store).len(), 2);
        assert!(nw_668_owed_debt(&fx.db_path).is_empty());
    }

    /// nw-668 review (I2): an owed retry whose drift could not be computed
    /// (`paths: None`) recomputes drift from disk vs graph — which cannot see
    /// notes whose text committed but whose links did not. Owing such notes
    /// must carry them alongside, not drop them.
    #[test]
    fn nw_668_owed_links_survive_merging_into_an_uncomputed_drift_retry() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget"]);
        let path = fx.root.join("n000.md");
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        // What the failed drift attempt that produced `existing` disclosed.
        crate::index_md::record_vault_reconciliation_debt(
            fx.store.db_path(),
            &fx.root,
            std::slice::from_ref(&fx.root),
            Some("drift failed"),
        );
        let existing = PendingReconciliation {
            paths: None,
            also_replay: Vec::new(),
            failures: 3,
            next_attempt: Instant::now() + Duration::from_secs(60),
        };
        let merged = watcher.owe_sidecar_retry(
            &fx.store,
            Some(existing),
            SidecarDebt {
                paths: vec![path.clone()],
                error: "injected".to_string(),
            },
        );
        assert!(merged.paths.is_none(), "still recomputes drift");
        assert_eq!(merged.also_replay, vec![path.clone()]);
        assert_eq!(merged.failures, 3, "an owed retry keeps its schedule");
        let rows: HashMap<String, String> =
            crate::index_md::load_skipped_notes_sidecar(&fx.db_path)
                .skipped
                .into_iter()
                .map(|file| (file.path, file.reason))
                .collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(
            rows[&fx.root.to_string_lossy().into_owned()]
                .contains("brain watcher startup reconciliation failed; retrying (drift failed)"),
            "the startup row keeps its own reason: {rows:?}"
        );
        assert!(
            rows[&path.to_string_lossy().into_owned()]
                .contains("brain watcher code links not yet written; retrying (injected)"),
            "{rows:?}"
        );
    }

    /// nw-668 review (M5): `sidecar_scan_skipped` is reachable — an edit that
    /// pushes a note over the size limit is not read (nw-469), the note keeps
    /// its stored body, and so it keeps the code links that body produced.
    #[test]
    fn nw_668_an_oversized_edit_keeps_its_previous_code_links() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget", "BravoWidget"]);
        let path = fx.root.join("n000.md");
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        fs::write(&path, "# A\n\nuses AlphaWidget\n").unwrap();
        watcher
            .process_batch(&fx.store, None, &fx.v_uid, vec![path.clone()], &None)
            .unwrap();
        let before = nw_668_targets(&fx.store);
        assert_eq!(before.len(), 2);

        let huge = format!(
            "# A\n\nuses BravoWidget\n{}",
            "x".repeat(crate::index_limits::DEFAULT_MAX_NOTE_BYTES as usize)
        );
        fs::write(&path, huge).unwrap();
        watcher
            .process_batch(&fx.store, None, &fx.v_uid, vec![path.clone()], &None)
            .unwrap();
        assert_eq!(
            watcher
                .last_batch_phase_timings()
                .unwrap()
                .sidecar_scan_skipped,
            1
        );
        assert_eq!(nw_668_targets(&fx.store), before);
    }

    /// nw-668 re-review (N1): owed link debt lives in the skipped-notes
    /// sidecar, but startup reconciliation only diffed disk against the
    /// graph's TEXT — which matches for a note whose links were owed — then
    /// cleared the vault's debt. A restart silently dropped the owed links.
    /// Startup must replay the persisted owed notes.
    #[test]
    fn nw_668_owed_links_survive_a_watcher_restart() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget"]);
        let path = fx.root.join("n000.md");
        fs::write(&path, "# A\n\nuses AlphaWidget\n").unwrap();
        // The text lands, the links do not (both flush attempts fail).
        let first = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(2));
        let owed = first.process_batch(&fx.store, None, &fx.v_uid, vec![path.clone()], &None);
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(0));
        let owed = owed.unwrap().expect("owed");
        crate::index_md::record_vault_link_debt(
            fx.store.db_path(),
            &fx.root,
            &owed.paths,
            &owed.error,
            false,
        );
        assert!(nw_668_targets(&fx.store).is_empty());
        drop(first);

        // The daemon restarts: a fresh watcher's startup reconciliation.
        let store = Arc::new(GraphStore::open_or_create(&fx.db_path).unwrap());
        drop(fx.store);
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        let stop = watcher.shutdown_handle();
        watcher
            .with_ready_callback(move || stop.stop())
            .run_with_store(store.clone(), None)
            .unwrap();
        assert_eq!(
            nw_668_targets(&store).len(),
            2,
            "owed links rebuilt at startup"
        );
        assert!(
            nw_668_owed_debt(&fx.db_path).is_empty(),
            "and the debt cleared"
        );
    }

    /// nw-668 re-review: a live batch that rebuilds an owed note's links
    /// settles its disclosure then, not only when the scheduled retry runs.
    #[test]
    fn nw_668_a_live_batch_that_lands_an_owed_notes_links_clears_its_row() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget"]);
        let owed = fx.root.join("n000.md");
        let other = fx.root.join("n001.md");
        fs::write(&owed, "# A\n\nuses AlphaWidget\n").unwrap();
        crate::index_md::record_vault_link_debt(
            fx.store.db_path(),
            &fx.root,
            &[owed.clone(), other.clone()],
            "earlier failure",
            false,
        );
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        watcher
            .process_batch(&fx.store, None, &fx.v_uid, vec![owed.clone()], &None)
            .unwrap();
        assert_eq!(
            nw_668_owed_debt(&fx.db_path),
            vec![other.to_string_lossy().into_owned()],
            "only the note whose links landed is settled"
        );
    }

    /// nw-668 re-review (M4): a note whose sections cannot be read is owed
    /// like a failed flush, with or without Tantivy — not a batch failure
    /// that ends the live watcher's run loop.
    #[test]
    fn nw_668_an_unreadable_note_structure_is_owed_even_with_tantivy() {
        let _guard = serial_watcher_test();
        let fx = nw_668_fixture(&[], 2, &["AlphaWidget"]);
        let tantivy = TantivyIndex::open_or_create(&fx.db_path.with_extension("tantivy")).unwrap();
        let path = fx.root.join("n000.md");
        let fine = fx.root.join("n001.md");
        fs::write(&path, "# A\n\nuses AlphaWidget zebra\n").unwrap();
        fs::write(&fine, "# B\n\nuses AlphaWidget yak\n").unwrap();
        let watcher = BrainWatcher::new(&fx.db_path, &fx.root, "default", "test");
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counted = Arc::clone(&calls);
        let callback: Option<Box<dyn Fn() + Send>> = Some(Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        FAIL_NOTE_STRUCTURE_READS.with(|fail| fail.set(1));
        let owed = watcher.process_batch(
            &fx.store,
            Some(&tantivy),
            &fx.v_uid,
            vec![path.clone(), fine.clone()],
            &callback,
        );
        FAIL_NOTE_STRUCTURE_READS.with(|fail| fail.set(0));
        let owed = owed.unwrap().expect("the unreadable note is owed");
        assert_eq!(owed.paths, vec![path.clone()]);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the batch still publishes");
        assert!(
            !tantivy.search("yak", 10).unwrap().is_empty(),
            "other notes land"
        );
        assert_eq!(
            nw_668_targets(&fx.store).len(),
            2,
            "the readable note's links land"
        );

        // The owed replay heals it, Tantivy included.
        let pending = watcher.owe_sidecar_retry(&fx.store, None, owed);
        let next = watcher
            .attempt_reconciliation(
                &fx.store,
                Some(&tantivy),
                &fx.v_uid,
                &None,
                pending.paths,
                pending.also_replay,
                pending.failures,
            )
            .unwrap();
        assert!(next.is_none());
        assert!(!tantivy.search("zebra", 10).unwrap().is_empty());
        assert_eq!(nw_668_targets(&fx.store).len(), 4);
    }
}
