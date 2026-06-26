//! File watcher for live incremental updates.
//!
//! Watches a single vault directory. When a markdown file is saved,
//! re-parses it, drops the old Note + descendants from the graph via
//! `delete_note_cascade`, and re-inserts the fresh data. Wikilinks
//! survive the cycle because `note_uid` is derived from
//! `(vault_uid, rel_path)` — content-stable — so any other note's
//! WIKILINK_TO_NOTE edges that pointed to this note get reattached on
//! the next reindex pass.
//!
//! Threading: synchronous + blocking. The caller owns the thread (the
//! CLI `brain watch` command runs it in the foreground; MCP integration
//! can spawn a dedicated thread once we've verified lbug's
//! multi-writer semantics).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use globset::GlobSet;
use nestweaver_parser::{ParsedNote, is_markdown, parse_markdown};
use nestweaver_schema::{
    Heading, Note, Section, Tag, Vault, heading_uid, note_uid, section_uid, tag_uid, vault_uid,
};
use nestweaver_store::{GraphScope, GraphStore, TantivyIndex};
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEvent, DebouncedEventKind, new_debouncer};

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

/// Names of directories the watcher never descends into.
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
}

impl BrainWatcher {
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

    /// Set the debounce interval for filesystem events.
    pub fn with_debounce_ms(mut self, ms: u64) -> Self {
        self.debounce_ms = ms;
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
        let store = Arc::new(
            GraphStore::open_or_create(&self.db_path)
                .with_context(|| format!("open GraphStore at {}", self.db_path.display()))?,
        );
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
        ensure_vault(
            &store,
            &v_uid,
            &self.vault_root,
            &self.instance_id,
            &self.vault_name,
        )?;

        // Channel from the debouncer into our loop.
        let (tx, rx) = std::sync::mpsc::channel::<DebounceResult>();
        let mut debouncer = new_debouncer(
            Duration::from_millis(self.debounce_ms),
            move |res: Result<Vec<DebouncedEvent>, notify::Error>| {
                let _ = tx.send(res);
            },
        )
        .with_context(|| "init debouncer")?;
        debouncer
            .watcher()
            .watch(&self.vault_root, RecursiveMode::Recursive)
            .with_context(|| format!("watch {}", self.vault_root.display()))?;

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
            let batch = match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Ok(events)) => events,
                Ok(Err(err)) => {
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
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
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
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("debouncer disconnected; exiting");
                    return Ok(());
                }
            };

            // Pre-build the symbol index once per batch so cross-domain
            // refresh doesn't re-query the DB for every file.
            let symbol_index = crate::cross_domain::build_symbol_index(&store).ok();

            // Pre-build the wikilink title lookup once per batch so
            // reinsert_note doesn't re-query all notes for every file.
            // Uses a bidirectional map: title→UIDs (forward) + UID→title
            // (reverse) for O(1) removal on rename.
            let mut title_forward: HashMap<String, Vec<String>> = HashMap::new();
            let mut title_reverse: HashMap<String, String> = HashMap::new();
            for n in store.list_notes(None).unwrap_or_default() {
                let key = n.title.to_lowercase();
                title_forward
                    .entry(key.clone())
                    .or_default()
                    .push(n.uid.clone());
                title_reverse.insert(n.uid.clone(), key);
            }

            let mut any_change = false;
            for event in batch {
                match self.handle_event(
                    &store,
                    tantivy.as_deref(),
                    &v_uid,
                    event,
                    symbol_index.as_ref(),
                    &mut title_forward,
                    &mut title_reverse,
                ) {
                    Ok(outcome) => {
                        if matches!(
                            outcome,
                            UpdateOutcome::Updated { .. } | UpdateOutcome::Deleted { .. }
                        ) {
                            any_change = true;
                        }
                        log_outcome(&outcome);
                    }
                    Err(e) => tracing::warn!("event handling failed: {e:#}"),
                }
            }

            // After a batch that touched the graph, recompute PPR over
            // the unified scope so brain_context queries see fresh ranks.
            // Per the architecture doc §6.3: full recompute is fine for
            // <50K-node graphs (~milliseconds); true incremental
            // forward-push residuals are a later optimisation.
            if any_change {
                match store.compute_pagerank(0.85, 20, &GraphScope::unified()) {
                    Ok(()) => {
                        tracing::debug!(
                            generation = store.pagerank_generation(),
                            "PPR recomputed after watcher batch"
                        );
                    }
                    Err(e) => tracing::warn!("post-batch PPR recompute failed: {e}"),
                }

                // Record the watcher commit timestamp so `brain status`
                // shows the actual last-indexed time, not max(modified_at).
                if let Err(e) = crate::extensions::record_last_indexed_at(&self.db_path, &v_uid) {
                    tracing::warn!("failed to record last_indexed_at: {e}");
                }

                // Bump the graph generation counter so consumers (e.g. the
                // web server SSE handler) can detect that the graph changed.
                // P0.2: also persist it to `<db>.generation` so short-lived
                // processes (and the F16 cache) see the bump after restart.
                let gen_sidecar = crate::sidecar_path(&self.db_path, ".generation");
                store.bump_and_persist_graph_generation(&gen_sidecar);
                if let Some(ref cb) = on_change {
                    cb();
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_event(
        &self,
        store: &GraphStore,
        tantivy: Option<&TantivyIndex>,
        v_uid: &str,
        event: DebouncedEvent,
        symbol_index: Option<&crate::cross_domain::SymbolIndex>,
        title_forward: &mut HashMap<String, Vec<String>>,
        title_reverse: &mut HashMap<String, String>,
    ) -> Result<UpdateOutcome, anyhow::Error> {
        let path = event.path;

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
                match crate::manifest::load_manifest_cache(manifests_path) {
                    Ok(mut cache) => {
                        cache.insert(repo_key.clone(), manifest);
                        if let Err(e) = crate::manifest::save_manifest_cache(&cache, manifests_path)
                        {
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
        let n_uid = note_uid(v_uid, &rel_path);

        // Reject symlinks whose target is outside the vault root.
        if path.is_symlink() {
            match std::fs::canonicalize(&path) {
                Ok(resolved) if !resolved.starts_with(&self.vault_root) => {
                    tracing::warn!("skipping symlink escaping vault root: {}", path.display());
                    return Ok(UpdateOutcome::Skipped {
                        path,
                        reason: "symlink target outside vault root",
                    });
                }
                Err(_) => {
                    return Ok(UpdateOutcome::Skipped {
                        path,
                        reason: "cannot resolve symlink",
                    });
                }
                Ok(_) => {}
            }
        }

        // Inspect the filesystem to distinguish modify/create vs delete.
        // notify-debouncer-mini collapses related events into a single
        // DebouncedEvent of kind Any; we always re-stat the path.
        let file_exists = path.exists();

        // Step 1: always cascade-delete the existing graph data for this
        // note. Safe even when the note doesn't yet exist (no-op).
        store
            .delete_note_cascade(&n_uid)
            .context("delete_note_cascade")?;
        if let Some(t) = tantivy
            && let Err(e) = t.remove_note(&n_uid)
        {
            tracing::warn!("tantivy.remove_note({n_uid}) failed: {e}");
        }

        if !file_exists {
            return Ok(UpdateOutcome::Deleted { path });
        }

        // Step 2: re-parse and re-insert.
        let source =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let parsed = parse_markdown(&rel_path, &source)?;

        let (headings, sections, wikilinks_count, tags_count) = reinsert_note(
            store,
            v_uid,
            &n_uid,
            &path,
            &rel_path,
            &parsed,
            event.kind,
            title_forward,
        )?;

        // Update bidirectional title lookup: O(1) removal via reverse map.
        if let Some(old_title) = title_reverse.remove(&n_uid)
            && let Some(uids) = title_forward.get_mut(&old_title)
        {
            uids.retain(|u| u != &n_uid);
            if uids.is_empty() {
                title_forward.remove(&old_title);
            }
        }
        let new_title = parsed.title.to_lowercase();
        title_forward
            .entry(new_title.clone())
            .or_default()
            .push(n_uid.clone());
        title_reverse.insert(n_uid.clone(), new_title);

        // Refresh cross-domain (Note↔Symbol) edges for this note. The
        // store's delete_note_cascade already DETACH-deleted any prior
        // REFERENCES_CODE_* edges, so we just need to re-emit fresh ones.
        let cd_result = if let Some(idx) = symbol_index {
            crate::cross_domain::discover_cross_domain_links_for_note_with_index(store, &n_uid, idx)
        } else {
            crate::cross_domain::discover_cross_domain_links_for_note(store, &n_uid)
        };
        if let Err(e) = cd_result {
            tracing::warn!("cross-domain refresh for {n_uid} failed: {e}");
        }

        // Mirror the update into Tantivy. Best-effort: log on failure.
        if let Some(t) = tantivy {
            let heading_docs: Vec<(String, String)> = parsed
                .headings
                .iter()
                .map(|h| (heading_uid(&n_uid, &h.slug, h.start_line), h.text.clone()))
                .collect();
            let section_docs: Vec<(String, String, String)> = parsed
                .sections
                .iter()
                .map(|s| {
                    let th = crate::hash::blake3_hex(&s.text);
                    let s_uid = section_uid(&n_uid, s.start_line, &th[..12]);
                    let heading_title = s
                        .heading_idx
                        .and_then(|i| parsed.headings.get(i))
                        .map(|h| h.text.clone())
                        .unwrap_or_default();
                    (s_uid, s.text.clone(), heading_title)
                })
                .collect();
            let body_chunks: Vec<String> = parsed.sections.iter().map(|s| s.text.clone()).collect();
            let tag_names: Vec<String> = parsed.tags.iter().map(|t| t.name.clone()).collect();
            if let Err(e) = t.update_note(
                &n_uid,
                &parsed.title,
                v_uid,
                &body_chunks,
                &heading_docs,
                &section_docs,
                &tag_names,
            ) {
                tracing::warn!("tantivy.update_note failed: {e}");
            }
        }

        Ok(UpdateOutcome::Updated {
            path,
            headings,
            sections,
            wikilinks: wikilinks_count,
            tags: tags_count,
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

/// The debouncer's callback receives `Result<Vec<DebouncedEvent>, notify::Error>`.
type DebounceResult = Result<Vec<DebouncedEvent>, notify::Error>;

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

/// Insert all derived nodes + edges for a single freshly-parsed note.
/// Returns (headings_count, sections_count, wikilinks_count, tags_count)
/// for logging. Wikilinks are written *only* to notes that already exist
/// in the DB at the time of this call — the watcher does not re-resolve
/// the full vault. For full multi-note resolution use
/// `index_markdown_directory`.
#[allow(clippy::too_many_arguments)]
fn reinsert_note(
    store: &GraphStore,
    v_uid: &str,
    n_uid: &str,
    path: &Path,
    rel_path: &str,
    parsed: &ParsedNote,
    _event_kind: DebouncedEventKind,
    title_lookup: &HashMap<String, Vec<String>>,
) -> Result<(usize, usize, usize, usize), anyhow::Error> {
    // ── Note + VAULT_HAS_NOTE ───────────────────────────────────────────
    let frontmatter_json = if parsed
        .frontmatter
        .as_object()
        .is_some_and(|m| !m.is_empty())
    {
        serde_json::to_string(&parsed.frontmatter).ok()
    } else {
        None
    };
    let (created_at, modified_at) = match std::fs::metadata(path) {
        Ok(meta) => {
            let c = meta.created().ok().and_then(format_system_time);
            let m = meta.modified().ok().and_then(format_system_time);
            (c, m)
        }
        Err(_) => (None, None),
    };

    store
        .insert_note(&Note {
            uid: n_uid.to_string(),
            vault_uid: v_uid.to_string(),
            file_path: rel_path.to_string(),
            title: parsed.title.clone(),
            note_kind: parsed.note_kind,
            word_count: parsed.word_count,
            content_hash: parsed.content_hash.clone(),
            frontmatter: frontmatter_json,
            created_at,
            modified_at,
            pagerank_score: None,
            embedding: None,
        })
        .context("insert_note")?;
    store
        .insert_vault_note_edge(v_uid, n_uid)
        .context("insert_vault_note_edge")?;

    // ── Headings + NOTE_HAS_HEADING + HEADING_PARENT ────────────────────
    let heading_uids: Vec<String> = parsed
        .headings
        .iter()
        .map(|h| heading_uid(n_uid, &h.slug, h.start_line))
        .collect();
    let mut headings: Vec<Heading> = Vec::with_capacity(parsed.headings.len());
    for (idx, h) in parsed.headings.iter().enumerate() {
        headings.push(Heading {
            uid: heading_uids[idx].clone(),
            note_uid: n_uid.to_string(),
            level: h.level,
            text: h.text.clone(),
            slug: h.slug.clone(),
            start_line: h.start_line,
            end_line: h.end_line,
            content_hash: crate::hash::blake3_hex_short(&h.text),
            embedding: None,
        });
    }
    store.batch_insert_headings(&headings)?;
    let nh_edges: Vec<(&str, &str)> = heading_uids.iter().map(|h| (n_uid, h.as_str())).collect();
    store.batch_insert_note_heading_edges(&nh_edges)?;

    let mut parent_edges: Vec<(String, String)> = Vec::new();
    for (idx, h) in parsed.headings.iter().enumerate() {
        for prev in (0..idx).rev() {
            if parsed.headings[prev].level < h.level {
                parent_edges.push((heading_uids[idx].clone(), heading_uids[prev].clone()));
                break;
            }
        }
    }
    let parent_refs: Vec<(&str, &str)> = parent_edges
        .iter()
        .map(|(c, p)| (c.as_str(), p.as_str()))
        .collect();
    store.batch_insert_heading_parent_edges(&parent_refs)?;

    // ── Sections + NOTE_HAS_SECTION + HEADING_HAS_SECTION ───────────────
    let mut sections: Vec<Section> = Vec::with_capacity(parsed.sections.len());
    let mut section_uids: Vec<String> = Vec::with_capacity(parsed.sections.len());
    let mut ns_edges: Vec<(String, String)> = Vec::new();
    let mut hs_edges: Vec<(String, String)> = Vec::new();
    for sec in &parsed.sections {
        let text_hash = crate::hash::blake3_hex(&sec.text);
        let s_uid = section_uid(n_uid, sec.start_line, &text_hash[..12]);
        let word_count = u32::try_from(sec.text.split_whitespace().count()).unwrap_or(u32::MAX);
        let heading_link = sec.heading_idx.map(|i| heading_uids[i].clone());
        sections.push(Section {
            uid: s_uid.clone(),
            note_uid: n_uid.to_string(),
            heading_uid: heading_link.clone(),
            start_line: sec.start_line,
            end_line: sec.end_line,
            text_hash,
            text_content: sec.text.clone(),
            word_count,
            pagerank_score: None,
        });
        ns_edges.push((n_uid.to_string(), s_uid.clone()));
        if let Some(h_uid) = heading_link {
            hs_edges.push((h_uid, s_uid.clone()));
        }
        section_uids.push(s_uid);
    }
    store.batch_insert_sections(&sections)?;
    let ns_refs: Vec<(&str, &str)> = ns_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store.batch_insert_note_section_edges(&ns_refs)?;
    let hs_refs: Vec<(&str, &str)> = hs_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store.batch_insert_heading_section_edges(&hs_refs)?;

    // ── Tags (deduplicate against existing Tag nodes, only insert new) ──
    let mut local_tag_uids: HashMap<String, String> = HashMap::new();
    let mut new_tag_nodes: Vec<Tag> = Vec::new();
    let mut note_tag_edges: Vec<(String, String)> = Vec::new();
    let mut section_tag_edges: Vec<(String, String)> = Vec::new();

    for raw in &parsed.tags {
        let canonical = raw.name.to_lowercase();
        let t_uid = local_tag_uids
            .entry(canonical.clone())
            .or_insert_with(|| {
                let uid = tag_uid(v_uid, &canonical);
                // Only push a new Tag node if the store doesn't already
                // have it — checking via list_tags would be O(N), but
                // we can rely on insert errors being caught by the
                // caller below. Simpler: always materialise the candidate,
                // attempt insert later, swallow PK-duplicate errors.
                new_tag_nodes.push(Tag {
                    uid: uid.clone(),
                    vault_uid: v_uid.to_string(),
                    name: canonical.clone(),
                });
                uid
            })
            .clone();
        match (raw.source, raw.section_idx) {
            (nestweaver_parser::TagSource::Frontmatter, _) => {
                note_tag_edges.push((n_uid.to_string(), t_uid));
            }
            (nestweaver_parser::TagSource::Inline, Some(idx)) if idx < section_uids.len() => {
                section_tag_edges.push((section_uids[idx].clone(), t_uid));
            }
            _ => {
                note_tag_edges.push((n_uid.to_string(), t_uid));
            }
        }
    }
    // Insert tag nodes one-at-a-time, ignoring PK-duplicate errors so
    // tags shared with other notes survive (LadybugDB enforces PK
    // uniqueness on insert).
    for t in &new_tag_nodes {
        if let Err(e) = store.insert_tag(t) {
            if e.is_duplicate() {
                tracing::debug!("insert_tag {} skipped (already exists): {e}", t.name);
            } else {
                tracing::warn!("insert_tag {} failed: {e}", t.name);
            }
        }
    }
    let nt_refs: Vec<(&str, &str)> = note_tag_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store.batch_insert_note_tag_edges(&nt_refs)?;
    let st_refs: Vec<(&str, &str)> = section_tag_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store.batch_insert_section_tag_edges(&st_refs)?;
    let tags_count = local_tag_uids.len();

    // ── Wikilinks (per-file: resolve against the pre-built title lookup) ─
    let mut wl_resolved = 0usize;
    let mut wl_note_edges: Vec<(String, String, f32, String)> = Vec::new();
    let mut wl_head_edges: Vec<(String, String, f32, String)> = Vec::new();

    for wl in &parsed.wikilinks {
        if wl.section_idx >= section_uids.len() {
            continue;
        }
        let source_section = &section_uids[wl.section_idx];
        let display = wl.display.clone().unwrap_or_else(|| wl.target.clone());
        let key = wl.target.to_lowercase();
        let Some(candidates) = title_lookup.get(&key) else {
            continue;
        };
        let n = candidates.len() as f32;
        let conf = if n == 1.0 { 1.0 } else { 1.0 / n };
        for target in candidates {
            if let Some(anchor) = &wl.heading_anchor {
                let anchor_slug = nestweaver_parser::markdown::slugify(anchor);
                if let Ok(headings) = store.headings_in_note(target)
                    && let Some(h) = headings.iter().find(|h| h.slug == anchor_slug)
                {
                    wl_head_edges.push((
                        source_section.clone(),
                        h.uid.clone(),
                        conf,
                        display.clone(),
                    ));
                    wl_resolved += 1;
                    continue;
                }
            }
            wl_note_edges.push((
                source_section.clone(),
                target.clone(),
                conf,
                display.clone(),
            ));
            wl_resolved += 1;
        }
    }

    let wl_note_refs: Vec<(&str, &str, f32, &str)> = wl_note_edges
        .iter()
        .map(|(s, n, c, d)| (s.as_str(), n.as_str(), *c, d.as_str()))
        .collect();
    store.batch_insert_wikilink_to_note_edges(&wl_note_refs)?;
    let wl_head_refs: Vec<(&str, &str, f32, &str)> = wl_head_edges
        .iter()
        .map(|(s, h, c, d)| (s.as_str(), h.as_str(), *c, d.as_str()))
        .collect();
    store.batch_insert_wikilink_to_heading_edges(&wl_head_refs)?;

    Ok((headings.len(), sections.len(), wl_resolved, tags_count))
}

/// Render a `SystemTime` as RFC 3339-ish UTC string. Mirrors index_md.rs.
fn format_system_time(t: std::time::SystemTime) -> Option<String> {
    let duration = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    let secs = duration.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y } as i32;
    Some(format!(
        "{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;

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
    fn skip_dir_detection() {
        let p = Path::new("/x/vault/.obsidian/workspace.json");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/x/vault/.git/HEAD");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/x/vault/notes/regular.md");
        assert!(!path_in_skip_dir(p));
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
        let (_dir, root) = make_vault(&[("seed.md", "# Seed\n\nbody\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");

        // Seed the DB by indexing once so the Vault node is set up.
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();

        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());

        // Give the watcher time to start before we mutate the tree.
        thread::sleep(Duration::from_millis(150));

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
        let (_dir, root) = make_vault(&[("note.md", "# Original Title\n\nbody\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");

        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();

        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());

        thread::sleep(Duration::from_millis(150));
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
        let (_dir, root) = make_vault(&[("keep.md", "# Keep\n"), ("doomed.md", "# Doomed\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");

        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        assert_eq!(
            GraphStore::open(&db_path).unwrap().count_notes().unwrap(),
            2
        );

        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());

        thread::sleep(Duration::from_millis(150));
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
        let (_dir, root) = make_vault(&[(".obsidian/config.json", "{}")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let before = GraphStore::open(&db_path).unwrap().count_notes().unwrap();

        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());

        thread::sleep(Duration::from_millis(150));
        fs::write(root.join(".obsidian/note-in-config.md"), "# X\n").unwrap();
        thread::sleep(Duration::from_millis(700));
        stop.stop();
        handle.join().unwrap().unwrap();

        let after = GraphStore::open(&db_path).unwrap().count_notes().unwrap();
        assert_eq!(before, after, ".obsidian/ files must not be indexed");
    }
}
