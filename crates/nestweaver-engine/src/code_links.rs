//! nw-675: keep every note's code links (REFERENCES_CODE edges) equal to what
//! its committed text says, whichever route changed the graph.
//!
//! Cross-domain discovery used to run only where a caller remembered to call
//! it: `brain add` on the direct route, the publication path, the server
//! worker, and the vault watcher for the notes it re-indexed. Two routes that
//! DROP links never called it:
//!
//! * a full `brain refresh` recreates every note of the vault, and the
//!   cascade takes the notes' code links with it;
//! * re-indexing a code repo deletes the changed files' symbols, and the
//!   cascade takes every note link INTO those files with it.
//!
//! Either way the links stayed gone until each note was edited again, and
//! nothing said so. Adding a discovery call to each such route is the
//! "remember the twin" fix the review rule warns about (CONTRIBUTING.md,
//! "sibling gaps"): the next route to drop links would forget it too.
//!
//! [`CodeLinkReconciler`] is level-triggered instead, like the trigram and
//! embedding reconcilers: it compares the edges each note HAS with the edges
//! its committed text and the current symbol index SAY it should have, and
//! rewrites only the notes that differ. It does not need to know which route
//! changed the graph, so a route added later cannot reintroduce the loss.
//!
//! Cost and locking (the [`nw-668`] rules): mentions are computed off the
//! write lease from the note's committed text (re-read from disk, checked
//! against the graph's content hash) and cached per content hash, so a pass
//! after a code change re-resolves cached mentions without reading any note.
//! Work goes in chunks of [`crate::cross_domain::NOTES_PER_TXN`] notes; each
//! chunk's actual edges are read with no lease held, and only a chunk that
//! has something to rewrite takes the lease, for one transaction.
//!
//! Disclosure: a route that is known to drop links records the debt with
//! [`mark_code_links_pending`] before it returns, and a pass that finds work
//! records it too. `brain status` shows the debt (through the skipped-notes
//! `reconciliation_pending` rows) until a complete pass settles it; a failed
//! pass keeps it, with its error, and is retried — never silently dropped.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};

use crate::config::CrossDomainConfig;
pub use crate::cross_domain::CROSS_DOMAIN_RULES_VERSION;
use crate::cross_domain::{
    CrossDomainResult, NOTES_PER_TXN, NoteMentions, ScannedNote, SectionSpan, flush_scanned_notes,
    note_mentions, resolve_note,
};

/// `<db>.code_links.json`: the code-link reconciler's durable state.
pub const CODE_LINKS_SIDECAR: &str = ".code_links.json";

/// Durable code-link debt and the last settled pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeLinksState {
    /// Set while some notes' code links may not match their text. Cleared
    /// only by a complete pass that started after the latest mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<CodeLinksPending>,
    /// Incremented by every [`mark_code_links_pending`]. A pass settles the
    /// debt only if no mark landed while it ran: a refresh that finished
    /// mid-pass may have dropped links the pass had already checked.
    #[serde(default)]
    pub marks: u64,
    /// When a pass last completed with the graph in agreement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<String>,
    /// nw-670 re-review F4: from the last completed pass — per vault root,
    /// notes changed on disk since indexing, left unlinked until refreshed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub changed_by_vault: std::collections::BTreeMap<String, usize>,
    /// nw-670 re-review F1: from the last completed pass — projects whose
    /// declared repos resolved to none, so their notes link unscoped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unscoped_projects: Vec<String>,
    /// nw-670: the link-rules version ([`CROSS_DOMAIN_RULES_VERSION`]) the
    /// stored links were built with; 0 (absent) is the pre-nw-670
    /// match-every-word rules. A mismatch makes the next pass a migration:
    /// remove every stored link in bounded batches, then rebuild all of
    /// them. Recorded only when that pass completes, so an interrupted
    /// migration starts over on the next pass (the purge is idempotent).
    #[serde(default)]
    pub rules_version: u32,
}

/// Why code links are owed, and how the last attempt to write them went.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeLinksPending {
    pub reason: String,
    pub since: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub failures: u32,
    /// Where a running pass has got to, when it has something to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
}

static CODE_LINKS_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn state_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, CODE_LINKS_SIDECAR)
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::extensions::format_epoch_secs(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Read `<db>.code_links.json`. Missing or unreadable is "nothing owed".
pub fn load_code_links_state(db_path: &Path) -> CodeLinksState {
    std::fs::read_to_string(state_path(db_path))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Read-modify-write the state under a process-wide lock; `update` returns
/// whether it changed anything. Written by temp file + rename, so a reader
/// never sees a torn file.
fn update_code_links_state(db_path: &Path, update: impl FnOnce(&mut CodeLinksState) -> bool) {
    let _guard = CODE_LINKS_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = load_code_links_state(db_path);
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
                tracing::warn!(path = %path.display(), %error, "failed to write code-link state");
            }
        }
        Err(error) => tracing::warn!(%error, "failed to serialize code-link state"),
    }
}

/// Record that notes' code links are owed because of `reason` (a route that
/// is known to drop them: a full vault refresh, a code re-index). Disclosed
/// in `brain status` until a complete [`CodeLinkReconciler`] pass that began
/// after this call settles it. An earlier debt keeps its `since`.
pub fn mark_code_links_pending(db_path: &Path, reason: &str) {
    update_code_links_state(db_path, |state| {
        state.marks = state.marks.wrapping_add(1);
        match &mut state.pending {
            Some(pending) => pending.reason = reason.to_string(),
            None => {
                state.pending = Some(CodeLinksPending {
                    reason: reason.to_string(),
                    since: now_iso(),
                    ..CodeLinksPending::default()
                })
            }
        }
        true
    });
}

/// Owe links for `reason` WITHOUT counting as a new mark: the pass that
/// records this is the one that will settle it (the rules migration).
fn owe_without_mark(db_path: &Path, reason: &str) {
    update_code_links_state(db_path, |state| {
        let pending = state.pending.get_or_insert_with(|| CodeLinksPending {
            since: now_iso(),
            ..CodeLinksPending::default()
        });
        pending.reason = reason.to_string();
        true
    });
}

/// Record how far a running pass has got, when links are owed.
fn record_progress(db_path: Option<&Path>, progress: String) {
    let Some(db_path) = db_path else {
        return;
    };
    update_code_links_state(db_path, |state| match &mut state.pending {
        Some(pending) => {
            pending.progress = Some(progress);
            true
        }
        None => false,
    });
}

/// nw-670 review L7: record that the stored links were built by the current
/// rules — for a caller that just linked EVERY note of a fresh graph with
/// bulk discovery (the staged publication), so the daemon's first pass does
/// not purge and rebuild links that are already current.
pub fn record_rules_version(db_path: &Path) {
    update_code_links_state(db_path, |state| {
        state.rules_version = CROSS_DOMAIN_RULES_VERSION;
        true
    });
}

/// Whether the stored links were built by other link rules — a migration
/// the next pass owes (nw-670 re-review R4: never delayed by pass spacing).
pub fn code_links_migration_owed(db_path: &Path) -> bool {
    load_code_links_state(db_path).rules_version != CROSS_DOMAIN_RULES_VERSION
}

/// Whether code links are currently owed.
pub fn code_links_pending(db_path: &Path) -> bool {
    load_code_links_state(db_path).pending.is_some()
}

/// The `code_links` object of `brain_status` (nw-670 review M3): whether
/// note→code links are owed, why, how far a running pass has got, the last
/// failure, and the link-rules version the stored links were built with
/// against the one this binary applies. Its own object, not rows in the
/// file-shaped skipped-notes channel: the debt is about links, not files.
pub fn code_links_status_json(db_path: Option<&Path>) -> serde_json::Value {
    let state = db_path.map(load_code_links_state).unwrap_or_default();
    let pending = state.pending.as_ref();
    serde_json::json!({
        "pending": pending.is_some(),
        "reason": pending.map(|p| p.reason.clone()),
        "since": pending.map(|p| p.since.clone()),
        "progress": pending.and_then(|p| p.progress.clone()),
        "last_error": pending.and_then(|p| p.last_error.clone()),
        "failures": pending.map(|p| p.failures).unwrap_or(0),
        "rules_version": state.rules_version,
        "current_rules_version": CROSS_DOMAIN_RULES_VERSION,
        "last_reconciled_at": state.last_reconciled_at,
        // nw-670 re-review F4: notes changed on disk since indexing have no
        // links until their vault is refreshed.
        "notes_changed_since_indexing": state
            .changed_by_vault
            .iter()
            .map(|(vault, count)| serde_json::json!({ "vault": vault, "count": count }))
            .collect::<Vec<_>>(),
        // nw-670 re-review F1: projects whose declared repos resolved to none.
        "unscoped_projects": state.unscoped_projects,
    })
}

/// The `code_links_pending` disclosure object (`reason`, `since`,
/// `progress`) while links are owed; `None` when nothing is.
pub fn code_links_pending_json(db_path: &Path) -> Option<serde_json::Value> {
    let pending = load_code_links_state(db_path).pending?;
    Some(serde_json::json!({
        "reason": pending.reason,
        "since": pending.since,
        "progress": pending.progress,
    }))
}

/// One human line for a `code_links_pending` object: the CLI text
/// renderers' twin of the JSON keys (nw-670 re-review R1).
pub fn code_links_text_note(pending: &serde_json::Value) -> String {
    let reason = pending
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("owed");
    let mut note = format!(
        "Note: note→code links are being rebuilt ({reason}); results may miss links notes should have"
    );
    if let Some(progress) = pending.get("progress").and_then(|v| v.as_str()) {
        note.push_str(&format!(" ({progress})"));
    }
    note
}

/// Ranked reads whose answers lean on note→code links (nw-670 review M2).
pub const CODE_LINK_RANKED_TOOLS: &[&str] = &["brain_context", "project_context", "investigate"];

/// nw-670 review M2: while note→code links are owed (a rules migration or a
/// relink after a refresh or re-index), a ranked answer may be missing links
/// its notes should have. Stamp `code_links_incomplete` (and why) onto the
/// response, like the watcher batch's `publication_in_progress`; remove it
/// when nothing is owed, so a cached answer never carries a stale one.
pub fn stamp_code_links_disclosure(db_path: &Path, value: &mut serde_json::Value) {
    let serde_json::Value::Object(map) = value else {
        return;
    };
    match code_links_pending_json(db_path) {
        Some(pending) => {
            map.insert("code_links_incomplete".to_string(), serde_json::json!(true));
            map.insert("code_links_pending".to_string(), pending);
        }
        None => {
            map.remove("code_links_incomplete");
            map.remove("code_links_pending");
        }
    }
}

/// Acquired around each chunk that rewrites edges; `None` on direct routes,
/// whose store already gives them exclusive write authority.
pub type CodeLinkLease<'a> = Option<&'a crate::watcher::WatchMutationLeaseFactory>;

/// What one reconciliation pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeLinkReconcileReport {
    /// Notes whose links were compared.
    pub notes_checked: usize,
    /// Notes that could not be checked: not readable from the local
    /// filesystem, or whose file no longer matches the text the graph
    /// committed (a pending edit the watcher or a refresh will index). Their
    /// existing links are left alone.
    pub notes_skipped: usize,
    /// Notes whose links differed and were rewritten, in pass order.
    pub rewritten: Vec<String>,
    /// Edges written for the rewritten notes.
    pub edges_written: usize,
    /// The pass stopped early (shutdown); the debt stays.
    pub stopped: bool,
    /// nw-670 review M5: notes of a local vault whose file could not be read
    /// (`path: error`, the first few). Unlike `notes_skipped`, these are
    /// notes the pass OWES links and could not check: the debt stays and is
    /// disclosed with them rather than settled as done.
    pub unreadable: Vec<String>,
    /// nw-670 re-review F4: per vault root, notes whose file changed (or
    /// vanished) since the graph indexed them. After a rules migration they
    /// have NO links until a refresh re-indexes them; disclosed, not owed.
    pub changed_by_vault: std::collections::BTreeMap<String, usize>,
    /// nw-670 re-review F1: projects whose declared repos resolved to none.
    pub unscoped_projects: Vec<String>,
}

/// How many unreadable notes a pass names (the count is uncapped).
const UNREADABLE_DISCLOSED: usize = 5;

/// What the reconciler could learn about a note's committed text.
enum NoteText {
    Mentions(Arc<NoteMentions>),
    /// No local vault directory: linked elsewhere (server worker) or never.
    NotLocal,
    /// The file no longer holds the committed text, or is gone: an edit or
    /// deletion the watcher or a refresh will index.
    Changed,
    /// The file of a local vault exists but could not be read (permission,
    /// invalid UTF-8): `path: error`.
    Unreadable(String),
}

/// One note's cached mentions, valid while its content hash is unchanged.
struct CachedMentions {
    content_hash: String,
    mentions: Arc<NoteMentions>,
}

/// Level-triggered note→code link reconciliation (see the module docs). Keep
/// one per database for the life of a process to reuse its mention cache.
pub struct CodeLinkReconciler {
    config: CrossDomainConfig,
    cache: HashMap<String, CachedMentions>,
}

/// An edge in comparable form: `(from_uid, symbol_uid, confidence × 10⁴)`.
type EdgeKey = (String, String, i64);

fn edge_key(from: &str, to: &str, confidence: f64) -> EdgeKey {
    (
        from.to_string(),
        to.to_string(),
        (confidence * 10_000.0).round() as i64,
    )
}

impl CodeLinkReconciler {
    pub fn new(config: CrossDomainConfig) -> Self {
        Self {
            config,
            cache: HashMap::new(),
        }
    }

    /// Run one pass: bring every locally readable note's code links in line
    /// with its committed text. Settles the durable debt when the pass
    /// completes and no new debt was recorded while it ran; records the
    /// failure (debt kept, error disclosed) when it does not.
    pub fn reconcile(
        &mut self,
        store: &GraphStore,
        lease: CodeLinkLease<'_>,
        should_stop: &dyn Fn() -> bool,
    ) -> Result<CodeLinkReconcileReport, anyhow::Error> {
        let db_path = store.db_path().map(Path::to_path_buf);
        let state_at_start = db_path
            .as_deref()
            .map(load_code_links_state)
            .unwrap_or_default();
        let migrating =
            db_path.is_some() && state_at_start.rules_version != CROSS_DOMAIN_RULES_VERSION;
        if migrating && let Some(db_path) = &db_path {
            owe_without_mark(
                db_path,
                &format!(
                    "note→code link rules upgraded to v{CROSS_DOMAIN_RULES_VERSION}: replacing \
                     every note's links"
                ),
            );
        }
        let outcome = self.reconcile_inner(store, lease, should_stop, migrating);
        let Some(db_path) = db_path else {
            return outcome;
        };
        // Every completed pass (settled or not) refreshes what it found about
        // stale notes and unscoped projects (nw-670 re-review F1/F4).
        if let Ok(report) = &outcome
            && !report.stopped
        {
            update_code_links_state(&db_path, |state| {
                let differs = state.changed_by_vault != report.changed_by_vault
                    || state.unscoped_projects != report.unscoped_projects;
                state.changed_by_vault = report.changed_by_vault.clone();
                state.unscoped_projects = report.unscoped_projects.clone();
                differs
            });
        }
        match &outcome {
            Ok(report) if report.stopped => {}
            Ok(report) if !report.unreadable.is_empty() => {
                // nw-670 review M5: not done — some notes could not be read.
                let message = format!(
                    "{} note(s) could not be read, e.g. {}",
                    report.unreadable.len(),
                    report.unreadable.join("; ")
                );
                update_code_links_state(&db_path, |state| {
                    state.rules_version = CROSS_DOMAIN_RULES_VERSION;
                    let pending = state.pending.get_or_insert_with(|| CodeLinksPending {
                        reason: "code links out of date".to_string(),
                        since: now_iso(),
                        ..CodeLinksPending::default()
                    });
                    pending.progress = None;
                    pending.last_error = Some(message);
                    pending.failures = pending.failures.saturating_add(1);
                    true
                });
            }
            Ok(_) => update_code_links_state(&db_path, |state| {
                // A completed pass built every checked note's links with
                // the current rules, whatever else is still owed.
                state.rules_version = CROSS_DOMAIN_RULES_VERSION;
                if state.marks == state_at_start.marks {
                    state.pending = None;
                    state.last_reconciled_at = Some(now_iso());
                } else if let Some(pending) = &mut state.pending {
                    // New debt landed mid-pass: the next pass settles it.
                    pending.progress = None;
                }
                true
            }),
            // nw-670 review L8: a lease refused because the daemon is
            // shutting down is a stop, not a failure to disclose.
            Err(error)
                if error
                    .downcast_ref::<crate::watcher::WatchMutationRefused>()
                    .is_some() => {}
            Err(error) => {
                let message = format!("{error:#}");
                update_code_links_state(&db_path, |state| {
                    let pending = state.pending.get_or_insert_with(|| CodeLinksPending {
                        reason: "code links out of date".to_string(),
                        since: now_iso(),
                        ..CodeLinksPending::default()
                    });
                    pending.last_error = Some(message);
                    pending.failures = pending.failures.saturating_add(1);
                    true
                });
            }
        }
        outcome
    }

    fn reconcile_inner(
        &mut self,
        store: &GraphStore,
        lease: CodeLinkLease<'_>,
        should_stop: &dyn Fn() -> bool,
        migrating: bool,
    ) -> Result<CodeLinkReconcileReport, anyhow::Error> {
        let mut report = CodeLinkReconcileReport::default();
        if migrating {
            if should_stop() {
                report.stopped = true;
                return Ok(report);
            }
            purge_links(store, lease)?;
        }
        let index = crate::cross_domain::build_symbol_index_with_config(store, &self.config)?;
        report.unscoped_projects = index.unscoped_projects.clone();
        if index.is_empty() {
            // No code indexed: there is nothing to link to, and no link can
            // exist (every edge ends at a Symbol).
            return Ok(report);
        }
        let (notes, integrity) = store
            .list_notes_with_integrity(None)
            .context("list notes for code-link reconciliation")?;
        report.notes_skipped += integrity.skipped_corrupt;
        let roots: HashMap<String, PathBuf> = store
            .list_vaults(None)
            .context("list vaults for code-link reconciliation")?
            .into_iter()
            .map(|vault| (vault.uid, PathBuf::from(vault.root_path)))
            .collect();
        let mut spans: HashMap<String, Vec<SectionSpan>> = HashMap::new();
        for (uid, note_uid, start_line, end_line) in store
            .list_section_spans()
            .context("list section spans for code-link reconciliation")?
        {
            spans.entry(note_uid).or_default().push(SectionSpan {
                uid,
                start_line,
                end_line,
            });
        }
        let live: HashSet<&str> = notes.iter().map(|note| note.uid.as_str()).collect();
        self.cache.retain(|uid, _| live.contains(uid.as_str()));

        let total = notes.len();
        for (done, chunk) in notes.chunks(NOTES_PER_TXN).enumerate() {
            if should_stop() {
                report.stopped = true;
                return Ok(report);
            }
            if migrating {
                record_progress(
                    store.db_path(),
                    format!(
                        "linking notes: {} of {total} checked",
                        (done * NOTES_PER_TXN).min(total)
                    ),
                );
            }
            let mut desired: Vec<(ScannedNote, String)> = Vec::with_capacity(chunk.len());
            for note in chunk {
                let mentions = match self.mentions_for(note, &roots) {
                    NoteText::Mentions(mentions) => mentions,
                    NoteText::NotLocal => {
                        report.notes_skipped += 1;
                        continue;
                    }
                    NoteText::Changed => {
                        report.notes_skipped += 1;
                        let vault = roots
                            .get(&note.vault_uid)
                            .map(|root| root.display().to_string())
                            .unwrap_or_else(|| note.vault_uid.clone());
                        *report.changed_by_vault.entry(vault).or_default() += 1;
                        continue;
                    }
                    NoteText::Unreadable(why) => {
                        report.notes_skipped += 1;
                        if report.unreadable.len() < UNREADABLE_DISCLOSED {
                            report.unreadable.push(why);
                        }
                        continue;
                    }
                };
                report.notes_checked += 1;
                let note_spans = spans.get(&note.uid).map(Vec::as_slice).unwrap_or(&[]);
                desired.push((
                    resolve_note(&note.uid, &mentions, note_spans, &index),
                    note.content_hash.clone(),
                ));
            }
            let uids: Vec<String> = desired
                .iter()
                .map(|(scanned, _)| scanned.note_uid().to_string())
                .collect();
            let mut actual: HashMap<String, HashSet<EdgeKey>> = HashMap::new();
            for (note_uid, from, to, confidence) in store
                .references_code_edges_for_notes(&uids)
                .context("read notes' code links")?
            {
                actual
                    .entry(note_uid)
                    .or_default()
                    .insert(edge_key(&from, &to, confidence));
            }
            let empty = HashSet::new();
            let dirty: Vec<(ScannedNote, String)> = desired
                .into_iter()
                .filter(|(scanned, _)| {
                    let want: HashSet<EdgeKey> = scanned
                        .edges()
                        .map(|(from, to, conf)| edge_key(from, to, f64::from(conf)))
                        .collect();
                    &want != actual.get(scanned.note_uid()).unwrap_or(&empty)
                })
                .collect();
            if dirty.is_empty() {
                continue;
            }
            self.rewrite(store, lease, dirty, &mut report)?;
        }
        Ok(report)
    }

    /// The note's mentions: cached while its content hash is unchanged,
    /// otherwise read from its file — only if that file still holds the text
    /// the graph committed, since the committed sections' line spans index
    /// into it. `None` when that text is not available locally.
    fn mentions_for(
        &mut self,
        note: &nestweaver_schema::Note,
        roots: &HashMap<String, PathBuf>,
    ) -> NoteText {
        if let Some(cached) = self.cache.get(&note.uid)
            && cached.content_hash == note.content_hash
        {
            return NoteText::Mentions(Arc::clone(&cached.mentions));
        }
        self.cache.remove(&note.uid);
        // A note with no local vault directory (a wiki note, a server-mode
        // bare clone) is not this reconciler's to link.
        let Some(root) = roots.get(&note.vault_uid).filter(|root| root.is_dir()) else {
            return NoteText::NotLocal;
        };
        let path = root.join(&note.file_path);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            // nw-670 re-review R2: a note deleted on disk but still in the
            // graph is a stale index (the watcher or the next refresh removes
            // it), exactly like an edited one — not links the pass owes.
            // Treating it as unreadable kept the debt open forever and
            // stamped every ranked read.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return NoteText::Changed;
            }
            Err(error) => return NoteText::Unreadable(format!("{}: {error}", path.display())),
        };
        if nestweaver_parser::note_content_hash(&source) != note.content_hash {
            return NoteText::Changed;
        }
        let mentions = Arc::new(note_mentions(&source));
        self.cache.insert(
            note.uid.clone(),
            CachedMentions {
                content_hash: note.content_hash.clone(),
                mentions: Arc::clone(&mentions),
            },
        );
        NoteText::Mentions(mentions)
    }

    /// Rewrite one chunk's differing notes: under the lease, in one
    /// transaction, inside a graph publication (generation and PageRank move
    /// with the edges, as they do for the watcher's own link writes). A note
    /// whose text changed since the scan is left for the next pass.
    fn rewrite(
        &self,
        store: &GraphStore,
        lease: CodeLinkLease<'_>,
        dirty: Vec<(ScannedNote, String)>,
        report: &mut CodeLinkReconcileReport,
    ) -> Result<(), anyhow::Error> {
        let uids: Vec<String> = dirty
            .iter()
            .map(|(scanned, _)| scanned.note_uid().to_string())
            .collect();
        let (_lease, publication) = begin_publication(store, lease)?;
        // Re-checked under the lease: a note edited since the scan has newer
        // text, and is left for the next pass.
        let current: HashMap<String, String> = store
            .lookup_notes_by_uids(&uids)
            .context("re-check notes before rewriting their code links")?
            .into_iter()
            .map(|note| (note.uid, note.content_hash))
            .collect();
        let batch: Vec<ScannedNote> = dirty
            .into_iter()
            .filter(|(scanned, hash)| current.get(scanned.note_uid()) == Some(hash))
            .map(|(scanned, _)| scanned)
            .collect();
        let mut result = CrossDomainResult::default();
        let flushed = flush_scanned_notes(store, &batch, &mut result).map(|_| !batch.is_empty());
        finish_publication(publication, flushed)?;
        report.edges_written += result.note_to_symbol_edges + result.section_to_symbol_edges;
        report
            .rewritten
            .extend(batch.iter().map(|scanned| scanned.note_uid().to_string()));
        Ok(())
    }
}

/// Take the lease, then — without blocking while holding it — the graph
/// publication: a publisher (the vault watcher, mid-batch) may own the
/// publication while it waits for the write lease, so on contention the
/// lease is released, the publication waited for, and both retried.
#[allow(clippy::type_complexity)]
pub(crate) fn begin_publication<'s>(
    store: &'s GraphStore,
    lease: CodeLinkLease<'_>,
) -> Result<
    (
        Option<Box<dyn crate::watcher::WatchMutationLease>>,
        crate::manifest::GraphMutationPublicationGuard<'s>,
    ),
    anyhow::Error,
> {
    loop {
        let guard = lease
            .map(|factory| factory("code_link_reconcile"))
            .transpose()?;
        if let Some(publication) = crate::manifest::try_begin_graph_mutation_publication(
            store,
            "code link reconciliation",
        )? {
            return Ok((guard, publication));
        }
        drop(guard);
        store.wait_until_index_publication_unowned();
    }
}

/// Publish a write made inside [`begin_publication`]: generation and
/// PageRank move with the edges, as they do for the watcher's own link
/// writes. `written` is whether anything changed, or the write's error.
pub(crate) fn finish_publication(
    publication: crate::manifest::GraphMutationPublicationGuard<'_>,
    written: Result<bool, anyhow::Error>,
) -> Result<(), anyhow::Error> {
    let changed = matches!(written, Ok(true));
    match publication.finish(changed) {
        Ok(outcome) => {
            for warning in &outcome.warnings {
                tracing::warn!(
                    stage = %warning.stage,
                    "code link reconciliation publication: {}",
                    warning.message
                );
            }
        }
        Err(error) => {
            if written.is_ok() {
                return Err(error.context("publish code link reconciliation"));
            }
            tracing::warn!(%error, "retire code link reconciliation publication");
        }
    }
    written.map(|_| ())
}

/// nw-670 migration: remove every stored link in ONE step — the two
/// REFERENCES_CODE tables are dropped and recreated
/// ([`GraphStore::truncate_references_code_edges`]) inside a single lease
/// and publication — then the pass relinks note by note.
///
/// Review H1: deleting the old rules' ~39 M edges in 50,000-edge batches
/// meant ~780 publications, each failing ranked reads closed while its
/// marker stood and each paying a full finalize, plus as many
/// delete+checkpoint cycles on the REL tables: hours on a real brain. The
/// truncate is a metadata change whose cost does not grow with the edges.
fn purge_links(store: &GraphStore, lease: CodeLinkLease<'_>) -> Result<(), anyhow::Error> {
    let started = std::time::Instant::now();
    let (_lease, publication) = begin_publication(store, lease)?;
    let truncated = store
        .truncate_references_code_edges()
        .map(|()| true)
        .map_err(|e| anyhow::anyhow!("truncate_references_code_edges: {e}"));
    finish_publication(publication, truncated)?;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "code link rules migration: removed every link built by the previous rules"
    );
    record_progress(
        store.db_path(),
        "removed every link built by the previous rules; relinking".to_string(),
    );
    Ok(())
}

/// One complete pass for a caller with exclusive write authority (the
/// direct CLI routes): no lease, no stop signal, a fresh mention cache.
pub fn reconcile_code_links(
    store: &GraphStore,
    config: &CrossDomainConfig,
) -> Result<CodeLinkReconcileReport, anyhow::Error> {
    CodeLinkReconciler::new(config.clone()).reconcile(store, None, &|| false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A vault of notes plus a Rust repo, indexed into one file-backed store
    /// the way `brain add` + `index` build one, with discovery run once.
    struct Fixture {
        _dir: TempDir,
        vault: PathBuf,
        repo: PathBuf,
        db: PathBuf,
        store: GraphStore,
    }

    const REPO_URL: &str = "file:///fixture/repo";

    fn fixture(notes: &[(&str, &str)], sources: &[(&str, &str)]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&vault).unwrap();
        for (path, text) in notes {
            std::fs::write(vault.join(path), text).unwrap();
        }
        for (path, text) in sources {
            let file = repo.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        let db = dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&vault, &db, "default", "vault").unwrap();
        let store = GraphStore::open_or_create(&db).unwrap();
        index_repo(&store, &repo, &db);
        // Bulk discovery and a first reconcile pass (the rules migration of
        // an empty link set) build the same links.
        crate::cross_domain::discover_cross_domain_links(&store).unwrap();
        let discovered = store.list_references_code_edges().unwrap();
        reconcile_code_links(&store, &CrossDomainConfig::default()).unwrap();
        assert_eq!(
            store.list_references_code_edges().unwrap(),
            discovered,
            "bulk discovery and the reconciler agree"
        );
        assert_eq!(
            load_code_links_state(&db).rules_version,
            CROSS_DOMAIN_RULES_VERSION
        );
        Fixture {
            _dir: dir,
            vault,
            repo,
            db,
            store,
        }
    }

    /// The code index route `index --repo` and the daemon's IndexRepo share.
    fn index_repo(store: &GraphStore, repo: &Path, db: &Path) {
        crate::index::index_directory_with_store(
            store, repo, db, "default", REPO_URL, "sha", false, None,
        )
        .unwrap();
    }

    /// The full vault refresh `brain refresh` (no `--since`) and `brain add`
    /// run through the daemon's IndexVault.
    fn full_vault_refresh(fx: &Fixture) {
        crate::index_md::index_markdown_directory_with_store_and_deletion_count_and_note_limits(
            &fx.store,
            &fx.vault,
            &fx.db,
            "default",
            "vault",
            &[],
            crate::index_limits::NoteLimits::default(),
        )
        .unwrap();
    }

    fn edges(store: &GraphStore) -> Vec<(String, String, f64, String)> {
        store.list_references_code_edges().unwrap()
    }

    fn note_uid_of(store: &GraphStore, file: &str) -> String {
        store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .find(|note| note.file_path == file)
            .unwrap()
            .uid
    }

    fn reconcile(store: &GraphStore) -> CodeLinkReconcileReport {
        reconcile_code_links(store, &CrossDomainConfig::default()).unwrap()
    }

    fn two_widgets() -> Fixture {
        fixture(
            &[
                ("a.md", "# A\n\nThe AlphaWidget renders.\n"),
                ("b.md", "# B\n\nThe BravoWidget renders.\n"),
            ],
            &[
                ("src/a.rs", "pub struct AlphaWidget;\n"),
                ("src/b.rs", "pub struct BravoWidget;\n"),
            ],
        )
    }

    /// nw-675, route 1: a full vault refresh recreates every note, and the
    /// cascade takes their code links with it. Nothing rediscovered them;
    /// they stayed gone until each note was edited. The reconciler puts back
    /// exactly the set discovery built.
    #[test]
    fn a_full_vault_refresh_drops_code_links_and_a_pass_restores_them() {
        let fx = two_widgets();
        let before = edges(&fx.store);
        assert_eq!(
            before.len(),
            4,
            "precondition: note + section edge per note"
        );

        full_vault_refresh(&fx);
        assert!(
            edges(&fx.store).is_empty(),
            "precondition (the bug): the refresh's cascade drops every code link"
        );

        let report = reconcile(&fx.store);
        assert_eq!(
            edges(&fx.store),
            before,
            "the pass restores discovery's edges"
        );
        assert_eq!(report.rewritten.len(), 2, "{report:?}");
    }

    /// nw-675, route 2: re-indexing a repo deletes the changed file's
    /// symbols, and the cascade takes the note links INTO that file with
    /// them. Counterweight: a note linking into an UNCHANGED file keeps its
    /// links and is not rewritten.
    #[test]
    fn a_code_reindex_drops_links_into_changed_files_and_a_pass_restores_only_those() {
        let fx = two_widgets();
        let a = note_uid_of(&fx.store, "a.md");
        let b = note_uid_of(&fx.store, "b.md");
        let links_of = |uid: &str| -> Vec<(String, String, f64, String)> {
            let sections: HashSet<String> = fx
                .store
                .sections_in_note(uid)
                .unwrap()
                .into_iter()
                .map(|section| section.uid)
                .collect();
            edges(&fx.store)
                .into_iter()
                .filter(|edge| edge.0 == uid || sections.contains(&edge.0))
                .collect()
        };
        let b_before = links_of(&b);
        assert_eq!(b_before.len(), 2);

        // Moving the struct down a line changes its uid (uids carry the
        // start line), so the re-index deletes the old symbol.
        std::fs::write(
            fx.repo.join("src/a.rs"),
            "// widgets\npub struct AlphaWidget;\n",
        )
        .unwrap();
        index_repo(&fx.store, &fx.repo, &fx.db);
        assert!(
            links_of(&a).is_empty(),
            "precondition (the bug): the re-index drops links into the changed file"
        );
        assert_eq!(links_of(&b), b_before, "precondition: b is unaffected");

        let report = reconcile(&fx.store);
        assert_eq!(report.rewritten, vec![a.clone()], "only a is rewritten");
        let a_after = links_of(&a);
        assert_eq!(a_after.len(), 2, "{a_after:?}");
        let alpha: Vec<String> = fx
            .store
            .list_all_symbols_lite()
            .unwrap()
            .into_iter()
            .filter(|(_, name, _)| name == "AlphaWidget")
            .map(|(uid, _, _)| uid)
            .collect();
        assert_eq!(alpha.len(), 1);
        assert!(a_after.iter().all(|edge| edge.1 == alpha[0]), "{a_after:?}");
        assert_eq!(links_of(&b), b_before, "b's links are untouched");
    }

    /// A pass over a graph that already agrees writes nothing and does not
    /// advance the generation (no publication for a no-op).
    #[test]
    fn a_pass_over_links_that_already_match_writes_nothing() {
        let fx = two_widgets();
        let generation = fx.store.graph_generation();
        let report = reconcile(&fx.store);
        assert!(report.rewritten.is_empty(), "{report:?}");
        assert_eq!(report.notes_checked, 2);
        assert_eq!(fx.store.graph_generation(), generation);
    }

    /// A rewrite is a publication: the generation moves, so a cached
    /// response keyed on it cannot outlive the old links.
    #[test]
    fn a_rewrite_advances_the_graph_generation() {
        let fx = two_widgets();
        full_vault_refresh(&fx);
        let generation = fx.store.graph_generation();
        reconcile(&fx.store);
        assert!(fx.store.graph_generation() > generation);
    }

    /// A note whose file no longer holds the text the graph committed is
    /// skipped — its sections' line spans index into the committed text —
    /// and its links are left alone. The watcher or a refresh indexes the
    /// edit, and a later pass links it.
    #[test]
    fn a_note_edited_since_it_was_indexed_is_skipped_not_misattributed() {
        let fx = two_widgets();
        full_vault_refresh(&fx);
        std::fs::write(fx.vault.join("a.md"), "# A\n\nNow the BravoWidget.\n").unwrap();
        let report = reconcile(&fx.store);
        assert_eq!(report.notes_skipped, 1, "{report:?}");
        assert_eq!(report.rewritten, vec![note_uid_of(&fx.store, "b.md")]);
    }

    fn status(db: &Path) -> serde_json::Value {
        code_links_status_json(Some(db))
    }

    fn skipped_rows(db: &Path) -> serde_json::Value {
        crate::index_md::skipped_notes_status_json(Some(db)).0["reconciliation_pending"].clone()
    }

    /// Owed links are disclosed from the moment a route records them until a
    /// complete pass lands them; a failed pass keeps the debt and says why.
    /// Review M3: in their own `code_links` object — never as a pseudo-file
    /// row in the skipped-notes reconciliation count.
    #[test]
    fn owed_code_links_are_disclosed_until_a_pass_lands_them() {
        let fx = two_widgets();
        assert_eq!(status(&fx.db)["pending"], false);

        full_vault_refresh(&fx);
        mark_code_links_pending(&fx.db, "full vault refresh");
        let owed = status(&fx.db);
        assert_eq!(owed["pending"], true, "{owed}");
        assert_eq!(owed["reason"], "full vault refresh", "{owed}");
        assert_eq!(skipped_rows(&fx.db), 0, "not a file row");

        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(1));
        let failed = reconcile_code_links(&fx.store, &CrossDomainConfig::default());
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(0));
        assert!(failed.is_err());
        let still = status(&fx.db);
        assert_eq!(still["pending"], true, "{still}");
        assert!(
            still["last_error"].as_str().unwrap().contains("injected"),
            "{still}"
        );
        assert_eq!(still["failures"], 1);

        reconcile(&fx.store);
        let settled = status(&fx.db);
        assert_eq!(settled["pending"], false, "{settled}");
        assert_eq!(settled["rules_version"], CROSS_DOMAIN_RULES_VERSION);
        assert_eq!(edges(&fx.store).len(), 4);
    }

    /// Review M2: a ranked answer carries `code_links_incomplete` while links
    /// are owed, and loses it (even a cached copy) once they land.
    #[test]
    fn ranked_answers_disclose_owed_code_links() {
        let fx = two_widgets();
        let mut answer = serde_json::json!({ "results": [] });
        stamp_code_links_disclosure(&fx.db, &mut answer);
        assert!(answer.get("code_links_incomplete").is_none(), "{answer}");

        mark_code_links_pending(&fx.db, "full vault refresh");
        stamp_code_links_disclosure(&fx.db, &mut answer);
        assert_eq!(answer["code_links_incomplete"], true);
        assert_eq!(answer["code_links_pending"]["reason"], "full vault refresh");

        reconcile(&fx.store);
        stamp_code_links_disclosure(&fx.db, &mut answer);
        assert!(answer.get("code_links_incomplete").is_none(), "{answer}");
        assert!(answer.get("code_links_pending").is_none(), "{answer}");
    }

    /// A route that drops links while a pass is running records debt the
    /// pass may already have checked past. That pass must not settle it.
    #[test]
    fn debt_recorded_during_a_pass_outlives_that_pass() {
        let fx = two_widgets();
        mark_code_links_pending(&fx.db, "first refresh");
        let db = fx.db.clone();
        let marked = std::cell::Cell::new(false);
        CodeLinkReconciler::new(CrossDomainConfig::default())
            .reconcile(&fx.store, None, &|| {
                if !marked.replace(true) {
                    mark_code_links_pending(&db, "second refresh");
                }
                false
            })
            .unwrap();
        assert!(
            code_links_pending(&fx.db),
            "the mid-pass mark is still owed"
        );

        reconcile(&fx.store);
        assert!(!code_links_pending(&fx.db), "a later pass settles it");
    }

    /// A pass stopped for shutdown keeps the debt for the next process.
    #[test]
    fn a_stopped_pass_keeps_the_debt() {
        let fx = two_widgets();
        full_vault_refresh(&fx);
        mark_code_links_pending(&fx.db, "full vault refresh");
        let report = CodeLinkReconciler::new(CrossDomainConfig::default())
            .reconcile(&fx.store, None, &|| true)
            .unwrap();
        assert!(report.stopped);
        assert!(code_links_pending(&fx.db));
        assert!(edges(&fx.store).is_empty());
    }

    /// The mention cache: a second pass over unchanged notes reads no file.
    /// Deleting the vault proves it — the notes are still linked from cache.
    #[test]
    fn a_later_pass_reuses_cached_mentions_without_reading_notes() {
        let fx = two_widgets();
        let mut reconciler = CodeLinkReconciler::new(CrossDomainConfig::default());
        reconciler.reconcile(&fx.store, None, &|| false).unwrap();
        std::fs::remove_dir_all(&fx.vault).unwrap();
        full_vault_refresh_ignoring_missing_vault(&fx);
        let report = reconciler.reconcile(&fx.store, None, &|| false).unwrap();
        assert_eq!(report.notes_skipped, 0, "{report:?}");
        assert_eq!(report.notes_checked, 2);
    }

    /// Write links the way the pre-nw-670 rules did — every word, every
    /// symbol — and a sidecar that predates the rules version, as an
    /// upgraded install finds them.
    fn plant_old_rule_links(fx: &Fixture) -> usize {
        let note = note_uid_of(&fx.store, "a.md");
        let symbols: Vec<String> = fx
            .store
            .list_all_symbols_lite()
            .unwrap()
            .into_iter()
            .map(|(uid, _, _)| uid)
            .collect();
        let rows: Vec<(&str, &str, f32, &str)> = symbols
            .iter()
            .map(|uid| (note.as_str(), uid.as_str(), 0.6, "name-match"))
            .collect();
        let conn = fx.store.begin_transaction().unwrap();
        GraphStore::delete_cross_domain_edges_for_notes_on(&conn, &[note.as_str()]).unwrap();
        GraphStore::batch_insert_note_to_symbol_edges_on(&conn, &rows).unwrap();
        fx.store.commit_transaction(&conn).unwrap();
        update_code_links_state(&fx.db, |state| {
            state.rules_version = 0;
            true
        });
        rows.len()
    }

    /// nw-670 migration: links built by the old rules are removed and every
    /// note relinked under the current ones; the new version is recorded and
    /// the migration was disclosed while it ran.
    #[test]
    fn a_rules_upgrade_replaces_old_links_and_records_the_version() {
        let fx = two_widgets();
        let current = edges(&fx.store);
        plant_old_rule_links(&fx);
        assert_ne!(edges(&fx.store), current, "precondition: old links planted");

        let seen = std::cell::RefCell::new(Vec::new());
        CodeLinkReconciler::new(CrossDomainConfig::default())
            .reconcile(&fx.store, None, &|| {
                seen.borrow_mut().push(status(&fx.db));
                false
            })
            .unwrap();
        assert_eq!(
            edges(&fx.store),
            current,
            "old links replaced by the rules'"
        );
        let state = load_code_links_state(&fx.db);
        assert_eq!(state.rules_version, CROSS_DOMAIN_RULES_VERSION);
        assert!(state.pending.is_none(), "{state:?}");
        let disclosed = seen.borrow();
        let reason = disclosed[0]["reason"].as_str().unwrap().to_string();
        assert!(reason.contains("rules upgraded"), "{reason}");
    }

    /// Counterweight: with the version current, the same planted links are
    /// a note that disagrees with its text and nothing more — only that note
    /// is rewritten, no purge runs.
    #[test]
    fn a_current_rules_version_runs_no_migration() {
        let fx = two_widgets();
        plant_old_rule_links(&fx);
        update_code_links_state(&fx.db, |state| {
            state.rules_version = CROSS_DOMAIN_RULES_VERSION;
            true
        });
        let report = reconcile(&fx.store);
        assert_eq!(report.rewritten, vec![note_uid_of(&fx.store, "a.md")]);
    }

    /// Plant `count` duplicate old-rule edges from note a to one symbol.
    fn plant_many_old_links(fx: &Fixture, count: usize) {
        let note = note_uid_of(&fx.store, "a.md");
        let symbol = fx.store.list_all_symbols_lite().unwrap().remove(0).0;
        let conn = fx.store.begin_transaction().unwrap();
        for chunk in (0..count).collect::<Vec<_>>().chunks(10_000) {
            let rows: Vec<(&str, &str, f32, &str)> = chunk
                .iter()
                .map(|_| (note.as_str(), symbol.as_str(), 0.6, "name-match"))
                .collect();
            GraphStore::batch_insert_note_to_symbol_edges_on(&conn, &rows).unwrap();
        }
        fx.store.commit_transaction(&conn).unwrap();
    }

    /// Review H1: the migration removes the old links in ONE publication
    /// whatever their number — it used to take one publication (and one
    /// fail-closed marker window, one full finalize) per 50,000 edges.
    #[test]
    fn the_migration_purge_is_one_publication_whatever_the_edge_count() {
        let fx = two_widgets();
        plant_old_rule_links(&fx);
        plant_many_old_links(&fx, 120_000);
        let before = fx.store.graph_generation();
        let at_relink = std::cell::Cell::new(None);
        let report = CodeLinkReconciler::new(CrossDomainConfig::default())
            .reconcile(&fx.store, None, &|| {
                if edges(&fx.store).is_empty() {
                    at_relink.set(Some(fx.store.graph_generation()));
                    return true;
                }
                false
            })
            .unwrap();
        assert!(report.stopped, "stopped at the relink, after the purge");
        assert_eq!(
            at_relink.get().map(|after| after - before),
            Some(1),
            "the whole purge advanced the generation once"
        );
    }

    /// Timing of the H1 purge on a graph with 1,000,000 old edges. Ignored
    /// by default (it plants a million edges); run with
    /// `cargo test -p nestweaver-engine --all-features --lib -- --ignored
    /// migration_purge_timing --nocapture`.
    #[test]
    #[ignore]
    fn migration_purge_timing_on_a_million_edges() {
        let fx = two_widgets();
        let planted = std::time::Instant::now();
        plant_many_old_links(&fx, 1_000_000);
        eprintln!("planted 1,000,000 edges in {:?}", planted.elapsed());
        let started = std::time::Instant::now();
        purge_links(&fx.store, None).unwrap();
        eprintln!("purged them in {:?}", started.elapsed());
        assert!(edges(&fx.store).is_empty());
        let relinked = std::time::Instant::now();
        reconcile(&fx.store);
        eprintln!("relinked in {:?}", relinked.elapsed());
        assert_eq!(edges(&fx.store).len(), 4);
    }

    /// An interrupted migration keeps its debt and its old version, and the
    /// next pass finishes it.
    #[test]
    fn an_interrupted_migration_resumes_on_the_next_pass() {
        let fx = two_widgets();
        let current = edges(&fx.store);
        plant_old_rule_links(&fx);
        let calls = std::cell::Cell::new(0);
        let report = CodeLinkReconciler::new(CrossDomainConfig::default())
            .reconcile(&fx.store, None, &|| {
                calls.set(calls.get() + 1);
                calls.get() > 1
            })
            .unwrap();
        assert!(report.stopped);
        let state = load_code_links_state(&fx.db);
        assert_eq!(
            state.rules_version, 0,
            "not recorded until a pass completes"
        );
        assert!(state.pending.is_some(), "still disclosed");

        reconcile(&fx.store);
        assert_eq!(edges(&fx.store), current);
        assert_eq!(
            load_code_links_state(&fx.db).rules_version,
            CROSS_DOMAIN_RULES_VERSION
        );
    }

    /// nw-670 R5 + `materialize_projects`: a note that joins a project is
    /// rescoped, and the next pass relinks it — `SharedWidget` is defined in
    /// two repos, so the unscoped note links nothing (cap 1), and once its
    /// project names one repo it links that repo's definition.
    #[test]
    fn a_project_membership_change_relinks_the_note() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("proj")).unwrap();
        std::fs::write(vault.join("proj/a.md"), "# A\n\nThe SharedWidget.\n").unwrap();
        let db = dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&vault, &db, "default", "vault").unwrap();
        let store = GraphStore::open_or_create(&db).unwrap();
        for name in ["alpha", "bravo"] {
            let repo = dir.path().join(name);
            std::fs::create_dir_all(repo.join("src")).unwrap();
            std::fs::write(repo.join("src/w.rs"), "pub struct SharedWidget;\n").unwrap();
            crate::index::index_directory_with_store(
                &store,
                &repo,
                &db,
                "default",
                &format!("file:///fixture/{name}"),
                "sha",
                false,
                Some(name),
            )
            .unwrap();
        }
        reconcile(&store);
        assert!(edges(&store).is_empty(), "ambiguous for an unscoped note");

        let config = crate::config::InstanceConfig::from_toml_str(
            r#"
instance_id = "default"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "p"
vault_folder = "proj"
repos = ["alpha"]
"#,
        )
        .unwrap();
        crate::project::materialize_projects(&store, &config, "default", &db).unwrap();
        let report = reconcile(&store);
        assert_eq!(report.rewritten.len(), 1, "{report:?}");
        let alpha_repo = nestweaver_schema::repo_uid("default", "file:///fixture/alpha");
        let linked = edges(&store);
        assert_eq!(linked.len(), 2, "{linked:?}");
        let symbols: HashMap<String, String> = store
            .list_symbols_for_linking()
            .unwrap()
            .into_iter()
            .map(|(uid, _, _, repo, _)| (uid, repo))
            .collect();
        assert!(
            linked.iter().all(|edge| symbols[&edge.1] == alpha_repo),
            "{linked:?}"
        );
    }

    /// nw-673: the configured `[cross_domain]` stoplist reaches the
    /// reconciler (it used to reach no route at all). Counterweight: the
    /// other note still links.
    #[test]
    fn the_configured_stoplist_reaches_the_reconciler() {
        let fx = two_widgets();
        full_vault_refresh(&fx);
        let config = crate::config::CrossDomainConfig {
            stoplist_extend: vec!["AlphaWidget".to_string()],
            ..Default::default()
        };
        reconcile_code_links(&fx.store, &config).unwrap();
        assert_eq!(
            edges(&fx.store).len(),
            2,
            "only b's links: AlphaWidget is stoplisted"
        );
    }

    /// nw-670 review M5: a note of a local vault whose file cannot be read
    /// is owed links the pass could not check. It is disclosed and the debt
    /// is NOT settled as done. Counterweight: once readable, a pass settles.
    #[test]
    fn an_unreadable_note_keeps_the_debt_and_is_disclosed() {
        let fx = two_widgets();
        full_vault_refresh(&fx);
        mark_code_links_pending(&fx.db, "full vault refresh");
        let file = fx.vault.join("a.md");
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        if std::fs::read_to_string(&file).is_ok() {
            // Running as root: permissions cannot make the file unreadable.
            return;
        }

        let report = reconcile(&fx.store);
        assert_eq!(report.unreadable.len(), 1, "{report:?}");
        let owed = code_links_status_json(Some(&fx.db));
        assert_eq!(owed["pending"], true, "{owed}");
        assert!(
            owed["last_error"]
                .as_str()
                .unwrap()
                .contains("could not be read"),
            "{owed}"
        );

        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
        reconcile(&fx.store);
        assert_eq!(code_links_status_json(Some(&fx.db))["pending"], false);
        assert_eq!(edges(&fx.store).len(), 4);
    }

    /// nw-670 re-review R2: a note deleted on disk but still in the graph
    /// is a stale index, skipped like an edited one; the debt settles rather
    /// than staying open until someone refreshes.
    #[test]
    fn a_note_deleted_on_disk_does_not_hold_the_debt_open() {
        let fx = two_widgets();
        full_vault_refresh(&fx);
        mark_code_links_pending(&fx.db, "full vault refresh");
        std::fs::remove_file(fx.vault.join("a.md")).unwrap();
        let report = reconcile(&fx.store);
        assert!(report.unreadable.is_empty(), "{report:?}");
        assert_eq!(report.notes_skipped, 1, "{report:?}");
        assert_eq!(code_links_status_json(Some(&fx.db))["pending"], false);
    }

    /// nw-670 review L8: a lease refused because the daemon is shutting
    /// down stops the pass without recording a failure.
    #[test]
    fn a_shutdown_refusal_is_not_recorded_as_a_failure() {
        let fx = two_widgets();
        full_vault_refresh(&fx);
        mark_code_links_pending(&fx.db, "full vault refresh");
        let refuse: crate::watcher::WatchMutationLeaseFactory =
            Arc::new(|_label| Err(anyhow::Error::new(crate::watcher::WatchMutationRefused)));
        let outcome = CodeLinkReconciler::new(CrossDomainConfig::default()).reconcile(
            &fx.store,
            Some(&refuse),
            &|| false,
        );
        assert!(outcome.is_err());
        let state = load_code_links_state(&fx.db);
        let pending = state.pending.expect("the debt stays");
        assert_eq!(pending.last_error, None, "{pending:?}");
        assert_eq!(pending.failures, 0);
    }

    /// nw-670 review L7: a graph just linked in full by bulk discovery
    /// records the current rules, so the next pass runs no migration.
    #[test]
    fn a_recorded_rules_version_skips_the_migration() {
        let fx = two_widgets();
        plant_old_rule_links(&fx);
        record_rules_version(&fx.db);
        let report = reconcile(&fx.store);
        assert_eq!(report.rewritten, vec![note_uid_of(&fx.store, "a.md")]);
    }

    /// nw-670 re-review F2: a NOTE seed's own code links come right after
    /// the seed in `connected`. Hybrid fusion capped a symbol reached only
    /// through PPR below every section and note that also matched BM25, so
    /// "context for this note" came back without the code it names.
    /// Counterweight: `a_symbol_seed_pins_no_code_links` (query.rs).
    #[test]
    fn a_note_seed_returns_the_code_it_links_first() {
        let mut notes: Vec<(String, String)> = vec![(
            "ingestion.md".to_string(),
            "# Ingestion engine\n\nThe ingestion engine uses `ClaimLedger`, `readCsvRows()` \
             and `runPipeline()`.\n\n## Ingestion flow\n\ningestion engine ingestion engine\n"
                .to_string(),
        )];
        for i in 0..20 {
            notes.push((
                format!("other{i}.md"),
                format!("# Ingestion engine notes {i}\n\ningestion engine ingestion engine {i}\n"),
            ));
        }
        let borrowed: Vec<(&str, &str)> = notes
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let fx = fixture(
            &borrowed,
            &[(
                "src/lib.rs",
                "pub struct ClaimLedger;\npub fn readCsvRows() {}\npub fn runPipeline() {}\n",
            )],
        );
        let tantivy_path = fx.db.with_extension("tantivy");
        let tantivy = nestweaver_store::TantivyIndex::open_or_create(&tantivy_path).unwrap();
        tantivy.reindex_from_store(&fx.store).unwrap();
        let linked: HashSet<String> = fx
            .store
            .list_symbols_for_linking()
            .unwrap()
            .into_iter()
            .map(|(uid, _, _, _, _)| uid)
            .collect();
        assert_eq!(
            edges(&fx.store)
                .iter()
                .filter(|e| e.0.starts_with("note:"))
                .count(),
            3
        );

        let result = crate::query::build_brain_context_hybrid(
            &fx.store,
            &["Ingestion engine".to_string()],
            Some(&tantivy),
            &crate::query::HybridSearchConfig::default(),
            None,
            None,
        )
        .unwrap();
        let top: Vec<&str> = result
            .connected
            .iter()
            .take(4)
            .map(|node| node.uid.as_str())
            .collect();
        assert_eq!(
            top.iter().filter(|uid| linked.contains(**uid)).count(),
            3,
            "the note's three linked symbols are in the top 4: {top:?}"
        );
    }

    /// A vault with `proj/a.md` naming `SharedWidget`, defined in repos
    /// `alpha` and `bravo`, and a config whose project `p` covers `proj`
    /// and declares `repos`.
    fn shared_widget_project(repos: &str) -> (tempfile::TempDir, PathBuf, PathBuf, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("proj")).unwrap();
        std::fs::write(vault.join("proj/a.md"), "# A\n\nThe SharedWidget.\n").unwrap();
        let db = dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&vault, &db, "default", "vault").unwrap();
        let store = GraphStore::open_or_create(&db).unwrap();
        for name in ["alpha", "bravo"] {
            let repo = dir.path().join(name);
            std::fs::create_dir_all(repo.join("src")).unwrap();
            std::fs::write(repo.join("src/w.rs"), "pub struct SharedWidget;\n").unwrap();
            crate::index::index_directory_with_store(
                &store,
                &repo,
                &db,
                "default",
                &format!("file:///fixture/{name}"),
                "sha",
                false,
                Some(name),
            )
            .unwrap();
        }
        let config = crate::config::InstanceConfig::from_toml_str(&format!(
            r#"
instance_id = "default"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "p"
vault_folder = "proj"
repos = [{repos}]
"#
        ))
        .unwrap();
        crate::project::materialize_projects(&store, &config, "default", &db).unwrap();
        (dir, vault, db, store)
    }

    /// nw-670 re-review F1: scoping read a project's repos only from its
    /// PROJECT_INCLUDES_SYMBOL edges, which were empty for every project on
    /// a real brain (nw-678), so every project note linked as unscoped. The
    /// declared repos (resolved by `materialize_projects`) scope it anyway.
    #[test]
    fn a_project_without_symbol_membership_still_scopes_its_notes() {
        let (_dir, _vault, _db, store) = shared_widget_project("\"alpha\"");
        let conn = store.begin_transaction().unwrap();
        conn.query("MATCH (:Project)-[r:PROJECT_INCLUDES_SYMBOL]->(:Symbol) DELETE r")
            .unwrap();
        store.commit_transaction(&conn).unwrap();
        reconcile(&store);
        let alpha_repo = nestweaver_schema::repo_uid("default", "file:///fixture/alpha");
        let symbols: HashMap<String, String> = store
            .list_symbols_for_linking()
            .unwrap()
            .into_iter()
            .map(|(uid, _, _, repo, _)| (uid, repo))
            .collect();
        let linked = edges(&store);
        assert_eq!(linked.len(), 2, "scoped to alpha: {linked:?}");
        assert!(linked.iter().all(|edge| symbols[&edge.1] == alpha_repo));
    }

    /// nw-670 re-review F1: a project whose declared repos resolve to none
    /// is disclosed, not silently linked unscoped.
    #[test]
    fn a_project_whose_declared_repos_resolve_to_none_is_disclosed() {
        let (_dir, _vault, db, store) = shared_widget_project("\"no-such-repo\"");
        reconcile(&store);
        let status = code_links_status_json(Some(&db));
        let project = nestweaver_schema::project_uid("default", "p");
        assert_eq!(
            status["unscoped_projects"],
            serde_json::json!([project]),
            "{status}"
        );
    }

    /// Counterweight: a project that declares no repos (a notes-only
    /// project) is not a gap.
    #[test]
    fn a_project_that_declares_no_repos_is_not_disclosed() {
        let (_dir, _vault, db, store) = shared_widget_project("");
        reconcile(&store);
        let status = code_links_status_json(Some(&db));
        assert_eq!(
            status["unscoped_projects"],
            serde_json::json!([]),
            "{status}"
        );
    }

    /// nw-670 re-review F4: notes changed on disk since indexing are left
    /// unlinked (their committed text is not on disk to scan); the count per
    /// vault is disclosed with the remedy.
    #[test]
    fn notes_changed_since_indexing_are_disclosed_per_vault() {
        let fx = two_widgets();
        std::fs::write(fx.vault.join("a.md"), "# A\n\nedited, not re-indexed\n").unwrap();
        reconcile(&fx.store);
        let status = code_links_status_json(Some(&fx.db));
        let changed = &status["notes_changed_since_indexing"];
        assert_eq!(changed.as_array().map(Vec::len), Some(1), "{status}");
        assert_eq!(changed[0]["count"], 1, "{status}");
    }

    /// Stand-in for a refresh that recreated the notes' links' absence: drop
    /// every code link directly (the vault is gone, so a real refresh would
    /// refuse to empty it).
    fn full_vault_refresh_ignoring_missing_vault(fx: &Fixture) {
        let uids: Vec<String> = fx
            .store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .map(|note| note.uid)
            .collect();
        let refs: Vec<&str> = uids.iter().map(String::as_str).collect();
        let conn = fx.store.begin_transaction().unwrap();
        GraphStore::delete_cross_domain_edges_for_notes_on(&conn, &refs).unwrap();
        fx.store.commit_transaction(&conn).unwrap();
        assert!(edges(&fx.store).is_empty());
    }
}
