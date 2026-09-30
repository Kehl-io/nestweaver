//! Keep name-inferred cross-repo call links (`CROSS_REPO_LINK` edges with
//! `cross_repo_name_*` evidence) equal to what a whole-graph inference says,
//! whichever route changed the code graph.
//!
//! A repository's index infers its OUTGOING links against the repositories
//! already present. Two gaps follow:
//!
//! * indexing A before B never links A to B, so the links depend on the
//!   order repositories were indexed in;
//! * re-indexing A deletes A's symbols, and the cascade takes every other
//!   repository's links INTO A with them. Those repositories are not
//!   re-indexed, so the links stay gone.
//!
//! The per-repository inference stays as the index's immediate answer. The
//! source of truth is [`reconcile_cross_repo_links`]: one inference over
//! every repository (`crate::index::infer_whole_graph_cross_repo_links`,
//! shared with the publication rebuild), which replaces all name-inferred
//! links in one transaction.
//!
//! Routes that change code record the debt with [`mark_cross_repo_links_owed`]
//! after they write. A route with exclusive write authority (`nestweaver
//! index`, the daemon's IndexRepo) pays it before it returns; the daemon's
//! watcher batches and server-mode fetches leave it to
//! [`run_cross_repo_link_relinker`], one debounced background pass. `brain
//! status` shows the debt (`cross_repo_links`) until a complete pass that
//! started after the latest mark settles it; a failed pass keeps it, with
//! its error.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};

/// `<db>.cross_repo_links.json`: the owed whole-graph inference.
pub const CROSS_REPO_LINKS_SIDECAR: &str = ".cross_repo_links.json";

/// Durable cross-repo link debt and the last settled pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossRepoLinksState {
    /// Set while the stored links may differ from a whole-graph inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<CrossRepoLinksPending>,
    /// Incremented by every [`mark_cross_repo_links_pending`]. A pass settles
    /// the debt only if no mark landed while it ran.
    #[serde(default)]
    pub marks: u64,
    /// When a pass last settled the debt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<String>,
}

/// Why the links are owed, and how the last attempt to rebuild them went.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossRepoLinksPending {
    pub reason: String,
    pub since: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub failures: u32,
}

static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn state_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, CROSS_REPO_LINKS_SIDECAR)
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::extensions::format_epoch_secs(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Read `<db>.cross_repo_links.json`. Missing or unreadable is "nothing owed".
pub fn load_cross_repo_links_state(db_path: &Path) -> CrossRepoLinksState {
    std::fs::read_to_string(state_path(db_path))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Read-modify-write under a process-wide lock; `update` returns whether it
/// changed anything. Temp file + rename, so a reader never sees a torn file.
fn update_state(db_path: &Path, update: impl FnOnce(&mut CrossRepoLinksState) -> bool) {
    let _guard = STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = load_cross_repo_links_state(db_path);
    if !update(&mut state) {
        return;
    }
    let path = state_path(db_path);
    match serde_json::to_vec_pretty(&state) {
        Ok(json) => {
            if let Err(error) =
                nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| {
                    std::io::Write::write_all(file, &json)
                })
            {
                tracing::warn!(path = %path.display(), %error, "failed to write cross-repo link state");
            }
        }
        Err(error) => tracing::warn!(%error, "failed to serialize cross-repo link state"),
    }
}

/// Record that the stored cross-repo links are owed a whole-graph pass
/// because of `reason`. An earlier debt keeps its `since`.
pub fn mark_cross_repo_links_pending(db_path: &Path, reason: &str) {
    update_state(db_path, |state| {
        state.marks = state.marks.wrapping_add(1);
        match &mut state.pending {
            Some(pending) => pending.reason = reason.to_string(),
            None => {
                state.pending = Some(CrossRepoLinksPending {
                    reason: reason.to_string(),
                    since: now_iso(),
                    ..CrossRepoLinksPending::default()
                })
            }
        }
        true
    });
}

/// Record the debt after a route changed code in `store`, when it can
/// matter: with fewer than two repositories there is nothing to link, and
/// removing the last other repository removed every link with it.
///
/// Recorded AFTER the write, so a pass already running when the write landed
/// is not the one that settles it.
pub fn mark_cross_repo_links_owed(store: &GraphStore, reason: &str) {
    let Some(db_path) = store.db_path() else {
        return;
    };
    let repos = match store.list_repos(None) {
        Ok(repos) => repos.len(),
        Err(error) => {
            tracing::warn!(%error, "cross-repo link debt: cannot list repositories; recording it");
            usize::MAX
        }
    };
    if repos >= 2 {
        mark_cross_repo_links_pending(db_path, reason);
    }
}

/// Whether a whole-graph pass is owed.
pub fn cross_repo_links_pending(db_path: &Path) -> bool {
    load_cross_repo_links_state(db_path).pending.is_some()
}

/// The `cross_repo_links` object of `brain_status`: whether name-inferred
/// cross-repo links are owed a whole-graph pass, why, since when, and the
/// last failure. Cross-repo impact, blast radius and affected tests may miss
/// links while `pending` is true.
pub fn cross_repo_links_status_json(db_path: Option<&Path>) -> serde_json::Value {
    let state = db_path.map(load_cross_repo_links_state).unwrap_or_default();
    let pending = state.pending.as_ref();
    serde_json::json!({
        "pending": pending.is_some(),
        "reason": pending.map(|p| p.reason.clone()),
        "since": pending.map(|p| p.since.clone()),
        "last_error": pending.and_then(|p| p.last_error.clone()),
        "failures": pending.map(|p| p.failures).unwrap_or(0),
        "last_reconciled_at": state.last_reconciled_at,
    })
}

/// Where a repository's indexed sources are re-read from when the parse
/// cache lacks a file.
pub(crate) struct RepoSource {
    pub(crate) reader: Box<dyn crate::content_reader::ContentReader>,
    /// The working tree, for a local repository; `None` for a bare clone.
    root: Option<PathBuf>,
}

impl RepoSource {
    /// The path the parser sees (it selects the language by extension).
    pub(crate) fn parse_path(&self, rel_path: &str) -> PathBuf {
        match &self.root {
            Some(root) => root.join(rel_path),
            None => PathBuf::from(rel_path),
        }
    }
}

/// Readers over `repo`'s indexed sources, in the order to try them: its
/// working tree, and a server-mode repository's bare clone
/// (`<db dir>/workspace/<name>.git`, as the worker pool lays them out) at the
/// revision it was indexed from. A repository with no recorded working tree
/// (server mode, whose `file://` URL may still name a directory that has
/// moved on) tries its bare clone first. The caller takes the first source
/// whose content still has the indexed hash.
pub(crate) fn repo_sources(
    repo: &nestweaver_schema::Repo,
    db_path: &Path,
    limits: crate::index_limits::IndexLimits,
) -> Vec<RepoSource> {
    let working_tree = repo
        .local_root()
        .map(PathBuf::from)
        .filter(|root| root.is_dir())
        .map(|root| RepoSource {
            reader: Box::new(crate::content_reader::FilesystemReader::with_limits(
                &root, limits,
            )),
            root: Some(root),
        });
    let bare = bare_clone_source(repo, db_path, limits);
    let server_mode = repo.root_path.as_deref().is_none_or(str::is_empty);
    let ordered = if server_mode {
        [bare, working_tree]
    } else {
        [working_tree, bare]
    };
    ordered.into_iter().flatten().collect()
}

fn bare_clone_source(
    repo: &nestweaver_schema::Repo,
    db_path: &Path,
    limits: crate::index_limits::IndexLimits,
) -> Option<RepoSource> {
    if repo.indexed_sha.is_empty() {
        return None;
    }
    let bare = db_path.parent()?.join("workspace").join(format!(
        "{}.git",
        crate::pull::clone_dir_name_from_url(&repo.url)
    ));
    if !crate::bare_clone::BareClone::is_valid_at(&bare)
        || crate::bare_clone::read_origin_url(&bare).ok().as_deref() != Some(repo.url.as_str())
    {
        return None;
    }
    Some(RepoSource {
        reader: Box::new(crate::content_reader::GitBareReader::with_limits(
            &bare,
            &repo.indexed_sha,
            limits,
        )),
        root: None,
    })
}

/// What one reconciliation pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossRepoReconcileReport {
    /// Links the whole-graph inference found.
    pub links: usize,
    /// Whether the stored links differed and were replaced.
    pub written: bool,
    pub repos: usize,
    pub files: usize,
    pub cache_hits: usize,
    pub reparsed: usize,
    /// The pass wrote its links and no mark landed while it ran: the debt
    /// (if any) is settled.
    pub settled: bool,
    /// Asked to stop before it wrote; nothing changed.
    pub stopped: bool,
    /// A code write landed while it inferred, so it did not write a result
    /// computed from the graph before that write; the debt stands.
    pub superseded: bool,
    pub elapsed_ms: u64,
}

/// One whole-graph pass: infer every name-matched cross-repo link with no
/// lease held, then take the lease (when given) for the one transaction
/// that replaces them, and settle the debt if no mark landed meanwhile.
///
/// Refuses before touching any link when a repository's parse cannot be
/// read ([`crate::index::CrossRepoInferenceInputsUnavailable`]); the error is
/// recorded on the debt and returned. A refusal while a newer code write
/// landed (a file saved after its batch) is not a failure: the pass is
/// superseded, and the next one reads the newer state.
pub fn reconcile_cross_repo_links(
    store: &GraphStore,
    db_path: &Path,
    limits: crate::index_limits::IndexLimits,
    lease: Option<&crate::watcher::WatchMutationLeaseFactory>,
    should_stop: &dyn Fn() -> bool,
) -> Result<CrossRepoReconcileReport, anyhow::Error> {
    let mut cache = crate::parsed_cache::ParsedCache::load(&parse_cache_path(db_path));
    reconcile_cross_repo_links_with_cache(store, db_path, limits, lease, should_stop, &mut cache)
}

fn parse_cache_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".parsed_cache.bin")
}

/// [`reconcile_cross_repo_links`] with a parse cache the caller keeps
/// between passes (refreshed from the cache's log, not reloaded).
pub fn reconcile_cross_repo_links_with_cache(
    store: &GraphStore,
    db_path: &Path,
    limits: crate::index_limits::IndexLimits,
    lease: Option<&crate::watcher::WatchMutationLeaseFactory>,
    should_stop: &dyn Fn() -> bool,
    cache: &mut crate::parsed_cache::ParsedCache,
) -> Result<CrossRepoReconcileReport, anyhow::Error> {
    let started = std::time::Instant::now();
    let marks_at_start = load_cross_repo_links_state(db_path).marks;
    // A graph write that landed after this is read makes the result stale,
    // whether or not its route has recorded its mark yet.
    let generation_at_start = store.graph_generation();
    let moved = || {
        load_cross_repo_links_state(db_path).marks != marks_at_start
            || store.graph_generation() != generation_at_start
    };
    let package_names = crate::manifest::package_names_hint(db_path);
    let inference = match crate::index::infer_whole_graph_cross_repo_links(
        store,
        db_path,
        &package_names,
        limits,
        should_stop,
        cache,
    ) {
        Ok(Some(inference)) => inference,
        Ok(None) => {
            return Ok(CrossRepoReconcileReport {
                stopped: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..CrossRepoReconcileReport::default()
            });
        }
        Err(error) if moved() => {
            tracing::debug!(
                error = %format!("{error:#}"),
                "cross-repo inference read state a newer write replaced; superseded"
            );
            return Ok(CrossRepoReconcileReport {
                superseded: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..CrossRepoReconcileReport::default()
            });
        }
        Err(error) => {
            record_failure(db_path, &error);
            return Err(error);
        }
    };
    let mut report = CrossRepoReconcileReport {
        links: inference.edges.len(),
        repos: inference.repos,
        files: inference.files,
        cache_hits: inference.cache_hits,
        reparsed: inference.reparsed,
        ..CrossRepoReconcileReport::default()
    };
    if should_stop() {
        report.stopped = true;
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        return Ok(report);
    }
    let mut edges = inference.edges;
    loop {
        let guard = lease
            .map(|factory| factory("cross_repo_links"))
            .transpose()?;
        if moved() {
            report.superseded = true;
            report.elapsed_ms = started.elapsed().as_millis() as u64;
            return Ok(report);
        }
        // An edge whose end has no Symbol node would be skipped by the
        // insert, so the stored set could never match and every pass would
        // rewrite it: keep only edges between existing symbols.
        keep_edges_between_existing_symbols(store, &mut edges)?;
        report.links = edges.len();
        // Re-checked under the lease: an unchanged set is not rewritten, so
        // a watcher batch that changed no linked name publishes nothing.
        let stored = store
            .list_inferred_cross_repo_links()
            .map_err(|e| anyhow::anyhow!("read inferred cross-repo links: {e}"))?;
        if same_links(stored, &edges) {
            break;
        }
        // A publication brackets the write, as for every other graph
        // mutation: generation and PageRank move with the edges. Another
        // publisher (a watcher) may own it while waiting for the write gate
        // this pass holds; yield the gate until it finishes, then retry.
        let Some(publication) = crate::manifest::try_begin_graph_mutation_publication(
            store,
            "cross-repo link inference",
        )?
        else {
            drop(guard);
            store.wait_until_index_publication_unowned();
            continue;
        };
        let written = store
            .replace_inferred_cross_repo_links(&edges)
            .map(|()| true)
            .map_err(|e| anyhow::anyhow!("replace inferred cross-repo links: {e}"));
        if let Err(error) = crate::code_links::finish_publication(publication, written) {
            record_failure(db_path, &error);
            return Err(error);
        }
        report.written = true;
        break;
    }
    report.settled = settle(db_path, marks_at_start);
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

fn keep_edges_between_existing_symbols(
    store: &GraphStore,
    edges: &mut Vec<nestweaver_schema::ResolvedEdge>,
) -> Result<(), anyhow::Error> {
    let mut uids: Vec<String> = edges
        .iter()
        .flat_map(|edge| [edge.source_uid.clone(), edge.target_uid.clone()])
        .collect();
    uids.sort();
    uids.dedup();
    if uids.is_empty() {
        return Ok(());
    }
    let existing: std::collections::HashSet<String> = store
        .lookup_symbols_by_uids(&uids)
        .map_err(|e| anyhow::anyhow!("look up linked symbols: {e}"))?
        .into_iter()
        .map(|symbol| symbol.uid)
        .collect();
    edges.retain(|edge| existing.contains(&edge.source_uid) && existing.contains(&edge.target_uid));
    Ok(())
}

/// Whether the stored name-inferred links are exactly `edges`, compared as
/// multisets of (source, target, confidence, link type, evidence) in the
/// form [`GraphStore::replace_inferred_cross_repo_links`] stores them.
fn same_links(
    stored: Vec<(String, String, f64, String, String)>,
    edges: &[nestweaver_schema::ResolvedEdge],
) -> bool {
    if stored.len() != edges.len() {
        return false;
    }
    let mut stored: Vec<_> = stored
        .into_iter()
        .map(|(source, target, confidence, link_type, evidence)| {
            (source, target, confidence.to_bits(), link_type, evidence)
        })
        .collect();
    let mut wanted: Vec<_> = edges
        .iter()
        .map(|edge| {
            (
                edge.source_uid.clone(),
                edge.target_uid.clone(),
                f64::from(edge.confidence).to_bits(),
                edge.link_type
                    .map(|link_type| format!("{link_type:?}"))
                    .unwrap_or_default(),
                if edge.evidence.is_empty() {
                    String::new()
                } else {
                    serde_json::to_string(&edge.evidence).unwrap_or_default()
                },
            )
        })
        .collect();
    stored.sort();
    wanted.sort();
    stored == wanted
}

/// Clear the debt when no mark landed since `marks_at_start`. Writes the
/// sidecar only when there was a debt, so a graph that never owed one (a
/// fresh publication slot) gains no file.
fn settle(db_path: &Path, marks_at_start: u64) -> bool {
    let mut settled = false;
    update_state(db_path, |state| {
        if state.marks != marks_at_start {
            return false;
        }
        settled = true;
        if state.pending.is_none() {
            return false;
        }
        state.pending = None;
        state.last_reconciled_at = Some(now_iso());
        true
    });
    settled
}

fn record_failure(db_path: &Path, error: &anyhow::Error) {
    let message = format!("{error:#}");
    update_state(db_path, |state| {
        let pending = state.pending.get_or_insert_with(|| CrossRepoLinksPending {
            reason: "whole-graph cross-repo inference failed".to_string(),
            since: now_iso(),
            ..CrossRepoLinksPending::default()
        });
        pending.last_error = Some(message);
        pending.failures = pending.failures.saturating_add(1);
        true
    });
}

/// The pass a route with exclusive write authority (the direct `nestweaver
/// index`) runs once at the end of its run. A failure is logged, not the
/// command's failure: the debt stays recorded and disclosed.
pub fn reconcile_after_index(
    store: &GraphStore,
    db_path: &Path,
    limits: crate::index_limits::IndexLimits,
    lease: Option<&crate::watcher::WatchMutationLeaseFactory>,
) -> Option<CrossRepoReconcileReport> {
    if !cross_repo_links_pending(db_path) {
        return None;
    }
    match reconcile_cross_repo_links(store, db_path, limits, lease, &|| false) {
        Ok(report) => {
            tracing::info!(
                links = report.links,
                repos = report.repos,
                files = report.files,
                reparsed = report.reparsed,
                elapsed_ms = report.elapsed_ms,
                settled = report.settled,
                "cross-repo links re-inferred over the whole graph"
            );
            Some(report)
        }
        Err(error) => {
            if error
                .downcast_ref::<crate::watcher::WatchMutationRefused>()
                .is_none()
            {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "whole-graph cross-repo inference failed; the links stay owed"
                );
            }
            None
        }
    }
}

/// How [`run_cross_repo_link_relinker`] waits.
#[derive(Debug, Clone, Copy)]
pub struct CrossRepoRelinkTiming {
    /// How often it reads the debt (a small sidecar read).
    pub tick: Duration,
    /// Quiet period: a pass starts once no new mark has landed for this long,
    /// so a burst of watcher batches coalesces into one pass.
    pub debounce: Duration,
    /// Upper bound on the wait under continuous marks.
    pub max_delay: Duration,
    /// Backoff after a failed pass, doubling up to `retry_max`.
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// How long the parse cache stays in memory after the last pass. Kept
    /// between the passes of a burst (a pass then reads only the cache's
    /// new log records), released when idle: a large brain's cache is
    /// hundreds of megabytes in memory.
    pub cache_idle: Duration,
}

impl Default for CrossRepoRelinkTiming {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            debounce: Duration::from_secs(3),
            max_delay: Duration::from_secs(60),
            retry_min: Duration::from_secs(30),
            retry_max: Duration::from_secs(10 * 60),
            cache_idle: Duration::from_secs(5 * 60),
        }
    }
}

/// The single background task that pays cross-repo link debt recorded by
/// watcher batches and server-mode fetches.
///
/// Level-triggered on the durable debt, so it needs no wake-up from the
/// routes. Its first pass is unconditional: it settles debt left by a
/// restart and links a crash dropped before its route could record the debt
/// (a pass that finds nothing to change writes nothing). After that it waits
/// for `debounce` without a new mark (at most `max_delay`), then runs one
/// pass off the async runtime: inference with no lease, the lease only for
/// the replace transaction. It never runs two passes at once, keeps its
/// parse cache between passes (reading only what other writers appended),
/// and backs off after a failure until the backoff ends or a new mark lands.
/// On shutdown it stops between repositories, and the lease factory refuses
/// (the pass returns without writing).
pub async fn run_cross_repo_link_relinker(
    store: Arc<GraphStore>,
    db_path: PathBuf,
    limits: crate::index_limits::IndexLimits,
    lease: crate::watcher::WatchMutationLeaseFactory,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    timing: CrossRepoRelinkTiming,
) {
    let mut tick = tokio::time::interval(timing.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The marks count last seen, and when it last moved.
    let mut seen: Option<(u64, tokio::time::Instant)> = None;
    let mut owed_since: Option<tokio::time::Instant> = None;
    let mut retry_after = timing.retry_min;
    // When to retry after a failure, and the marks count it failed at.
    let mut retry: Option<(tokio::time::Instant, u64)> = None;
    let mut startup = true;
    let mut cache = Some(crate::parsed_cache::ParsedCache::empty());
    let mut last_pass: Option<tokio::time::Instant> = None;
    loop {
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => return,
        }
        let state = load_cross_repo_links_state(&db_path);
        let now = tokio::time::Instant::now();
        if !startup {
            if state.pending.is_none() {
                seen = None;
                owed_since = None;
                if last_pass.is_some_and(|at| now.duration_since(at) >= timing.cache_idle) {
                    cache = Some(crate::parsed_cache::ParsedCache::empty());
                    last_pass = None;
                }
                continue;
            }
            let owed_at = *owed_since.get_or_insert(now);
            let quiet_since = match seen {
                Some((marks, at)) if marks == state.marks => at,
                _ => {
                    seen = Some((state.marks, now));
                    now
                }
            };
            if let Some((at, failed_marks)) = retry {
                // A new mark is new work, not the failure retried.
                if state.marks != failed_marks {
                    retry = None;
                } else if now < at {
                    continue;
                }
            }
            if now.duration_since(quiet_since) < timing.debounce
                && now.duration_since(owed_at) < timing.max_delay
            {
                continue;
            }
        }
        startup = false;
        let (pass_store, pass_db, pass_lease, stop) = (
            Arc::clone(&store),
            db_path.clone(),
            Arc::clone(&lease),
            shutdown.clone(),
        );
        let mut pass_cache = cache
            .take()
            .unwrap_or_else(crate::parsed_cache::ParsedCache::empty);
        let outcome = tokio::task::spawn_blocking(move || {
            let outcome = reconcile_cross_repo_links_with_cache(
                &pass_store,
                &pass_db,
                limits,
                Some(&pass_lease),
                &|| *stop.borrow(),
                &mut pass_cache,
            );
            (outcome, pass_cache)
        })
        .await;
        last_pass = Some(tokio::time::Instant::now());
        let outcome = match outcome {
            Ok((outcome, pass_cache)) => {
                cache = Some(pass_cache);
                Ok(outcome)
            }
            Err(join_error) => Err(join_error),
        };
        match outcome {
            Ok(Ok(report)) => {
                tracing::info!(
                    links = report.links,
                    written = report.written,
                    repos = report.repos,
                    files = report.files,
                    reparsed = report.reparsed,
                    elapsed_ms = report.elapsed_ms,
                    settled = report.settled,
                    superseded = report.superseded,
                    "cross-repo link relink pass"
                );
                if report.stopped {
                    return;
                }
                retry = None;
                retry_after = timing.retry_min;
                if report.settled {
                    owed_since = None;
                }
            }
            Ok(Err(error)) => {
                if error
                    .downcast_ref::<crate::watcher::WatchMutationRefused>()
                    .is_some()
                {
                    return;
                }
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "cross-repo link relink failed; the debt stays disclosed and is retried"
                );
                retry = Some((tokio::time::Instant::now() + retry_after, state.marks));
                retry_after = retry_after.saturating_mul(2).min(timing.retry_max);
            }
            Err(join_error) => {
                tracing::error!(%join_error, "cross-repo link relink task panicked");
                retry = Some((tokio::time::Instant::now() + retry_after, state.marks));
                retry_after = retry_after.saturating_mul(2).min(timing.retry_max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Two repositories that call into each other.
    struct Fixture {
        _dir: tempfile::TempDir,
        db: PathBuf,
        alpha: PathBuf,
    }

    fn write_sources(alpha: &Path, beta: &Path) {
        fs::create_dir_all(alpha.join("src")).unwrap();
        fs::create_dir_all(beta.join("src")).unwrap();
        fs::write(
            alpha.join("src/helper.js"),
            "export function alphaHelper() { return 1; }\nexport function alphaUses() {\n  return betaUtil();\n}\n",
        )
        .unwrap();
        fs::write(
            beta.join("src/caller.js"),
            "export function betaCaller() {\n  return alphaHelper();\n}\nexport function betaUtil() { return 2; }\n",
        )
        .unwrap();
    }

    fn index(root: &Path, db: &Path, url: &str) {
        crate::index::index_directory_with_opts(
            root,
            db,
            &crate::index::IndexOptions::new("test", url, "sha").force(true),
        )
        .unwrap();
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        write_sources(&alpha, &beta);
        let db = dir.path().join("graph.lbug");
        index(&alpha, &db, "file:///fixture/alpha");
        index(&beta, &db, "file:///fixture/beta");
        Fixture {
            _dir: dir,
            db,
            alpha,
        }
    }

    fn links(store: &GraphStore) -> Vec<(String, String, u64, String, String)> {
        let mut links: Vec<_> = store
            .list_inferred_cross_repo_links()
            .unwrap()
            .into_iter()
            .map(|(s, t, c, l, e)| (s, t, c.to_bits(), l, e))
            .collect();
        links.sort();
        links
    }

    fn names(store: &GraphStore) -> Vec<(String, String)> {
        let mut names: Vec<_> = store
            .list_all_cross_repo_links(1000)
            .unwrap()
            .into_iter()
            .map(|link| (link.source_name, link.target_name))
            .collect();
        names.sort();
        names
    }

    fn reconcile(store: &GraphStore, db: &Path) -> Result<CrossRepoReconcileReport, anyhow::Error> {
        reconcile_cross_repo_links(
            store,
            db,
            crate::index_limits::IndexLimits::default(),
            None,
            &|| false,
        )
    }

    fn both_directions() -> Vec<(String, String)> {
        vec![
            ("alphaUses".to_string(), "betaUtil".to_string()),
            ("betaCaller".to_string(), "alphaHelper".to_string()),
        ]
    }

    #[test]
    fn the_pass_links_both_directions_and_settles_the_debt() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        // Alpha was indexed before beta existed: only beta -> alpha.
        assert_eq!(
            names(&store),
            vec![("betaCaller".to_string(), "alphaHelper".to_string())]
        );
        mark_cross_repo_links_pending(&fx.db, "test");
        assert_eq!(cross_repo_links_status_json(Some(&fx.db))["pending"], true);
        let report = reconcile(&store, &fx.db).unwrap();
        assert!(report.written && report.settled, "{report:?}");
        assert_eq!(report.links, 2);
        assert_eq!(names(&store), both_directions());
        let status = cross_repo_links_status_json(Some(&fx.db));
        assert_eq!(status["pending"], false, "{status}");
        assert!(status["last_reconciled_at"].is_string(), "{status}");
    }

    /// A pass whose result equals the stored links writes nothing: no
    /// publication, no generation change.
    #[test]
    fn an_unchanged_result_is_not_rewritten() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        reconcile(&store, &fx.db).unwrap();
        let generation = store.graph_generation();
        let report = reconcile_cross_repo_links(
            &store,
            &fx.db,
            crate::index_limits::IndexLimits::default(),
            None,
            &|| false,
        )
        .unwrap();
        assert!(!report.written && report.settled, "{report:?}");
        assert_eq!(store.graph_generation(), generation);
    }

    /// The ordinary pass takes each parse from the cache, re-parses from the
    /// working tree when the cache is lost or corrupt, and writes those
    /// parses back, so the next pass reads no file.
    #[test]
    fn a_lost_or_corrupt_cache_is_reparsed_and_refilled() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        let first = reconcile(&store, &fx.db).unwrap();
        assert_eq!(first.reparsed, 0, "{first:?}");
        assert_eq!(first.cache_hits, first.files, "{first:?}");
        let expected = links(&store);

        let cache = crate::sidecar_path(&fx.db, ".parsed_cache.bin");
        for damage in [None, Some(b"not a parse cache".as_slice())] {
            match damage {
                None => fs::remove_file(&cache).unwrap(),
                Some(bytes) => fs::write(&cache, bytes).unwrap(),
            }
            // The previous round's re-parses went to the log: lose it too
            // (or, the second time, leave it torn).
            let log = crate::parsed_cache::log_path(&cache);
            match damage {
                None => {
                    let _ = fs::remove_file(&log);
                }
                Some(bytes) => fs::write(&log, bytes).unwrap(),
            }
            // Drop the links so the pass has something to restore.
            store.replace_inferred_cross_repo_links(&[]).unwrap();
            let report = reconcile(&store, &fx.db).unwrap();
            assert_eq!(report.reparsed, report.files, "{report:?}");
            assert!(report.written, "{report:?}");
            assert_eq!(links(&store), expected);
            let again = reconcile(&store, &fx.db).unwrap();
            assert_eq!(again.reparsed, 0, "the re-parses were cached: {again:?}");
            assert_eq!(links(&store), expected);
        }
    }

    /// Without a cache entry and with the indexed content gone, the pass
    /// refuses before touching a link, and the debt stays with its error.
    #[test]
    fn unreadable_inputs_refuse_and_keep_the_links_and_the_debt() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        reconcile(&store, &fx.db).unwrap();
        let before = links(&store);
        fs::remove_file(crate::sidecar_path(&fx.db, ".parsed_cache.bin")).unwrap();
        fs::write(
            fx.alpha.join("src/helper.js"),
            "export function somethingElse() { return 3; }\n",
        )
        .unwrap();
        mark_cross_repo_links_pending(&fx.db, "test");
        let error = reconcile(&store, &fx.db).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::index::CrossRepoInferenceInputsUnavailable>()
                .is_some(),
            "{error:#}"
        );
        assert_eq!(links(&store), before);
        let status = cross_repo_links_status_json(Some(&fx.db));
        assert_eq!(status["pending"], true, "{status}");
        assert_eq!(status["failures"], 1, "{status}");
        assert!(
            status["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("changed since it was indexed")),
            "{status}"
        );
    }

    /// A code write that lands while the pass infers makes its result stale:
    /// it writes nothing and the debt stands for the next pass.
    #[test]
    fn a_mark_during_the_pass_supersedes_it() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        mark_cross_repo_links_pending(&fx.db, "first");
        let before = links(&store);
        let marked = std::cell::Cell::new(false);
        let report = reconcile_cross_repo_links(
            &store,
            &fx.db,
            crate::index_limits::IndexLimits::default(),
            None,
            &|| {
                if !marked.replace(true) {
                    mark_cross_repo_links_pending(&fx.db, "second");
                }
                false
            },
        )
        .unwrap();
        assert!(report.superseded && !report.settled, "{report:?}");
        assert_eq!(links(&store), before);
        assert_eq!(cross_repo_links_status_json(Some(&fx.db))["pending"], true);
    }

    /// Counterweight: repositories that do not call each other get no links,
    /// though the pass ran over both.
    #[test]
    fn repositories_without_cross_calls_get_no_links() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        fs::create_dir_all(one.join("src")).unwrap();
        fs::create_dir_all(two.join("src")).unwrap();
        fs::write(
            one.join("src/a.js"),
            "export function oneOnly() { return localOne() + phantomShared(); }\nexport function localOne() { return 1; }\n",
        )
        .unwrap();
        fs::write(
            two.join("src/b.js"),
            "export function twoOnly() { return localTwo(); }\nexport function localTwo() { return 2; }\n",
        )
        .unwrap();
        let db = dir.path().join("graph.lbug");
        index(&one, &db, "file:///fixture/one");
        index(&two, &db, "file:///fixture/two");
        mark_cross_repo_links_pending(&db, "test");
        let store = GraphStore::open(&db).unwrap();
        // A public symbol of repository one that its parse does not define
        // (so the local-name filter cannot see it): one's call to it is a
        // same-repository call, which only the repository filter excludes.
        let mut phantom = store
            .list_all_symbols()
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.name == "localOne")
            .unwrap();
        phantom.name = "phantomShared".to_string();
        phantom.uid = format!("{}-phantom", phantom.uid);
        phantom.visibility = nestweaver_schema::Visibility::Public;
        store.insert_symbol(&phantom).unwrap();
        let report = reconcile(&store, &db).unwrap();
        assert_eq!(report.repos, 2, "{report:?}");
        assert!(report.files >= 2 && report.settled, "{report:?}");
        assert_eq!(report.links, 0);
        assert!(links(&store).is_empty());
    }

    /// One repository is never marked: there is nothing to link to.
    #[test]
    fn a_single_repository_records_no_debt() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        write_sources(&alpha, &beta);
        let db = dir.path().join("graph.lbug");
        index(&alpha, &db, "file:///fixture/alpha");
        mark_cross_repo_links_owed(&GraphStore::open(&db).unwrap(), "only one");
        assert!(!cross_repo_links_pending(&db));
        index(&beta, &db, "file:///fixture/beta");
        mark_cross_repo_links_owed(&GraphStore::open(&db).unwrap(), "two now");
        assert!(cross_repo_links_pending(&db));
    }

    /// The relinker pays recorded debt after the quiet period, and stops on
    /// shutdown.
    #[tokio::test]
    async fn the_relinker_pays_debt_after_the_debounce() {
        let fx = fixture();
        let store = Arc::new(GraphStore::open(&fx.db).unwrap());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let lease: crate::watcher::WatchMutationLeaseFactory =
            Arc::new(|_| Ok(Box::new(()) as Box<dyn crate::watcher::WatchMutationLease>));
        let task = tokio::spawn(run_cross_repo_link_relinker(
            Arc::clone(&store),
            fx.db.clone(),
            crate::index_limits::IndexLimits::default(),
            lease,
            shutdown_rx,
            CrossRepoRelinkTiming {
                tick: Duration::from_millis(20),
                debounce: Duration::from_millis(300),
                max_delay: Duration::from_secs(30),
                retry_min: Duration::from_millis(50),
                retry_max: Duration::from_millis(50),
                ..Default::default()
            },
        ));
        // The unconditional startup pass links both directions first.
        for _ in 0..200 {
            if names(&store) == both_directions() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(names(&store), both_directions());
        store.replace_inferred_cross_repo_links(&[]).unwrap();
        mark_cross_repo_links_pending(&fx.db, "test");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            cross_repo_links_pending(&fx.db),
            "no pass inside the debounce"
        );
        assert!(names(&store).is_empty());
        let mut settled = false;
        for _ in 0..200 {
            if !cross_repo_links_pending(&fx.db) {
                settled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(settled, "the relinker settles the debt");
        assert_eq!(names(&store), both_directions());
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the relinker stops on shutdown")
            .unwrap();
    }

    /// An incremental re-index adds its fresh parses to the cache, so the
    /// pass that follows re-reads no file.
    #[test]
    fn an_incremental_index_leaves_nothing_to_reparse() {
        let fx = fixture();
        fs::write(
            fx.alpha.join("src/helper.js"),
            "export function alphaHelper() { return 1; }\nexport function alphaUses() {\n  return betaUtil();\n}\n// edited\n",
        )
        .unwrap();
        crate::index::incremental_index(&fx.alpha, &fx.db, "test", "file:///fixture/alpha")
            .unwrap();
        let store = GraphStore::open(&fx.db).unwrap();
        let report = reconcile(&store, &fx.db).unwrap();
        assert_eq!(report.reparsed, 0, "{report:?}");
        assert!(report.cache_hits >= 2, "{report:?}");
    }

    /// A refusal while a newer write landed is the newer state winning, not
    /// a failure: no error, no recorded failure, the debt stands.
    #[test]
    fn a_refusal_after_a_newer_mark_is_superseded_not_failed() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        fs::remove_file(crate::sidecar_path(&fx.db, ".parsed_cache.bin")).unwrap();
        fs::write(
            fx.alpha.join("src/helper.js"),
            "export function savedAfterTheBatch() { return 3; }\n",
        )
        .unwrap();
        mark_cross_repo_links_pending(&fx.db, "batch");
        let marked = std::cell::Cell::new(false);
        let report = reconcile_cross_repo_links(
            &store,
            &fx.db,
            crate::index_limits::IndexLimits::default(),
            None,
            &|| {
                if !marked.replace(true) {
                    mark_cross_repo_links_pending(&fx.db, "the next batch");
                }
                false
            },
        )
        .unwrap();
        assert!(report.superseded, "{report:?}");
        let status = cross_repo_links_status_json(Some(&fx.db));
        assert_eq!(status["pending"], true, "{status}");
        assert_eq!(status["failures"], 0, "{status}");
    }

    /// A graph write that lands during inference supersedes the pass even
    /// before its route records the mark.
    #[test]
    fn a_graph_write_during_the_pass_supersedes_it() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        mark_cross_repo_links_pending(&fx.db, "test");
        let before = links(&store);
        let bumped = std::cell::Cell::new(false);
        let report = reconcile_cross_repo_links(
            &store,
            &fx.db,
            crate::index_limits::IndexLimits::default(),
            None,
            &|| {
                if !bumped.replace(true) {
                    store.bump_graph_generation();
                }
                false
            },
        )
        .unwrap();
        assert!(report.superseded && !report.written, "{report:?}");
        assert_eq!(links(&store), before);
    }

    /// An inferred edge whose source symbol is gone from the graph (the
    /// insert would skip it) is dropped before comparing, so the next pass
    /// finds the stored links current instead of rewriting them forever.
    #[test]
    fn edges_from_missing_symbols_do_not_rewrite_every_pass() {
        let fx = fixture();
        let store = GraphStore::open(&fx.db).unwrap();
        let beta_uid = store
            .list_repos(None)
            .unwrap()
            .into_iter()
            .find(|repo| repo.url.ends_with("beta"))
            .unwrap()
            .uid;
        store
            .delete_symbols_in_file(&beta_uid, "src/caller.js")
            .unwrap();
        let first = reconcile(&store, &fx.db).unwrap();
        assert_eq!(first.links, 0, "{first:?}");
        let second = reconcile(&store, &fx.db).unwrap();
        assert!(!second.written, "{second:?}");
    }

    /// The relinker's first pass is unconditional: links a crash dropped
    /// before any debt was recorded are restored at startup.
    #[tokio::test]
    async fn the_relinker_repairs_unrecorded_loss_at_startup() {
        let fx = fixture();
        let store = Arc::new(GraphStore::open(&fx.db).unwrap());
        assert!(!cross_repo_links_pending(&fx.db));
        assert_eq!(names(&store).len(), 1, "only beta -> alpha before a pass");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_cross_repo_link_relinker(
            Arc::clone(&store),
            fx.db.clone(),
            crate::index_limits::IndexLimits::default(),
            no_lease(),
            shutdown_rx,
            fast_timing(Duration::from_secs(3600)),
        ));
        let mut repaired = false;
        for _ in 0..200 {
            if names(&store) == both_directions() {
                repaired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(repaired, "{:?}", names(&store));
        let _ = shutdown_tx.send(true);
        task.await.unwrap();
    }

    /// A failure backs off, but a new mark is new work: it runs at once
    /// instead of waiting out the backoff.
    #[tokio::test]
    async fn a_new_mark_ends_a_failure_backoff() {
        let fx = fixture();
        let store = Arc::new(GraphStore::open(&fx.db).unwrap());
        let helper = fx.alpha.join("src/helper.js");
        let original = fs::read_to_string(&helper).unwrap();
        fs::remove_file(crate::sidecar_path(&fx.db, ".parsed_cache.bin")).unwrap();
        fs::write(&helper, "export function changed() { return 3; }\n").unwrap();
        mark_cross_repo_links_pending(&fx.db, "first");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_cross_repo_link_relinker(
            Arc::clone(&store),
            fx.db.clone(),
            crate::index_limits::IndexLimits::default(),
            no_lease(),
            shutdown_rx,
            fast_timing(Duration::from_secs(3600)),
        ));
        let failures = || cross_repo_links_status_json(Some(&fx.db))["failures"].as_u64();
        let mut failed = false;
        for _ in 0..200 {
            if failures() >= Some(1) {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(failed, "the first pass refuses");
        fs::write(&helper, original).unwrap();
        mark_cross_repo_links_pending(&fx.db, "second");
        let mut settled = false;
        for _ in 0..200 {
            if !cross_repo_links_pending(&fx.db) {
                settled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            settled,
            "the new mark ran a pass inside the one-hour backoff"
        );
        let _ = shutdown_tx.send(true);
        task.await.unwrap();
    }

    fn no_lease() -> crate::watcher::WatchMutationLeaseFactory {
        Arc::new(|_| Ok(Box::new(()) as Box<dyn crate::watcher::WatchMutationLease>))
    }

    fn fast_timing(retry: Duration) -> CrossRepoRelinkTiming {
        CrossRepoRelinkTiming {
            tick: Duration::from_millis(20),
            debounce: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            retry_min: retry,
            retry_max: retry,
            cache_idle: Duration::from_secs(60),
        }
    }
}
