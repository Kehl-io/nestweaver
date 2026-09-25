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
                    last_error: None,
                    failures: 0,
                })
            }
        }
        true
    });
}

/// Whether code links are currently owed.
pub fn code_links_pending(db_path: &Path) -> bool {
    load_code_links_state(db_path).pending.is_some()
}

/// The `brain status` row for owed code links, if any: `(path, reason)` in
/// the shape of the skipped-notes `reconciliation_pending_notes` rows.
pub(crate) fn code_links_status_row(db_path: &Path) -> Option<(String, String)> {
    let pending = load_code_links_state(db_path).pending?;
    let mut reason = format!(
        "note code links being rebuilt since {} ({})",
        pending.since, pending.reason
    );
    if let Some(error) = &pending.last_error {
        reason.push_str(&format!(
            "; last attempt failed ({} time(s)): {error}; retrying",
            pending.failures
        ));
    }
    Some(("note→code links".to_string(), reason))
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
        let marks_at_start = db_path
            .as_deref()
            .map(|db| load_code_links_state(db).marks)
            .unwrap_or_default();
        let outcome = self.reconcile_inner(store, lease, should_stop);
        let Some(db_path) = db_path else {
            return outcome;
        };
        match &outcome {
            Ok(report) if report.stopped => {}
            Ok(_) => update_code_links_state(&db_path, |state| {
                if state.marks != marks_at_start {
                    // New debt landed mid-pass: the next pass settles it.
                    return false;
                }
                state.pending = None;
                state.last_reconciled_at = Some(now_iso());
                true
            }),
            Err(error) => {
                let message = format!("{error:#}");
                update_code_links_state(&db_path, |state| {
                    let pending = state.pending.get_or_insert_with(|| CodeLinksPending {
                        reason: "code links out of date".to_string(),
                        since: now_iso(),
                        last_error: None,
                        failures: 0,
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
    ) -> Result<CodeLinkReconcileReport, anyhow::Error> {
        let mut report = CodeLinkReconcileReport::default();
        let index = crate::cross_domain::build_symbol_index_with_config(store, &self.config)?;
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

        for chunk in notes.chunks(NOTES_PER_TXN) {
            if should_stop() {
                report.stopped = true;
                return Ok(report);
            }
            let mut desired: Vec<(ScannedNote, String)> = Vec::with_capacity(chunk.len());
            for note in chunk {
                let Some(mentions) = self.mentions_for(note, &roots) else {
                    report.notes_skipped += 1;
                    continue;
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
    ) -> Option<Arc<NoteMentions>> {
        if let Some(cached) = self.cache.get(&note.uid)
            && cached.content_hash == note.content_hash
        {
            return Some(Arc::clone(&cached.mentions));
        }
        self.cache.remove(&note.uid);
        let root = roots.get(&note.vault_uid)?;
        let source = std::fs::read_to_string(root.join(&note.file_path)).ok()?;
        if nestweaver_parser::note_content_hash(&source) != note.content_hash {
            return None;
        }
        let mentions = Arc::new(note_mentions(&source));
        self.cache.insert(
            note.uid.clone(),
            CachedMentions {
                content_hash: note.content_hash.clone(),
                mentions: Arc::clone(&mentions),
            },
        );
        Some(mentions)
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
        loop {
            let _lease = lease
                .map(|factory| factory("code_link_reconcile"))
                .transpose()?;
            let current: HashMap<String, String> = store
                .lookup_notes_by_uids(&uids)
                .context("re-check notes before rewriting their code links")?
                .into_iter()
                .map(|note| (note.uid, note.content_hash))
                .collect();
            let Some(publication) = crate::manifest::try_begin_graph_mutation_publication(
                store,
                "code link reconciliation",
            )?
            else {
                // A publisher (the vault watcher, mid-batch) owns the
                // publication and may be waiting for the write lease: yield
                // it, wait, retry — never block on one while holding the
                // other.
                drop(_lease);
                store.wait_until_index_publication_unowned();
                continue;
            };
            let batch: Vec<ScannedNote> = dirty
                .into_iter()
                .filter(|(scanned, hash)| current.get(scanned.note_uid()) == Some(hash))
                .map(|(scanned, _)| scanned)
                .collect();
            let mut result = CrossDomainResult::default();
            let flushed = flush_scanned_notes(store, &batch, &mut result);
            let changed = flushed.is_ok() && !batch.is_empty();
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
                    if flushed.is_ok() {
                        return Err(error.context("publish code link reconciliation"));
                    }
                    tracing::warn!(%error, "retire code link reconciliation publication");
                }
            }
            flushed?;
            report.edges_written += result.note_to_symbol_edges + result.section_to_symbol_edges;
            report
                .rewritten
                .extend(batch.iter().map(|scanned| scanned.note_uid().to_string()));
            return Ok(());
        }
    }
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
        crate::cross_domain::discover_cross_domain_links(&store).unwrap();
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
        crate::index_md::skipped_notes_status_json(Some(db)).0
    }

    /// Owed links are disclosed from the moment a route records them until a
    /// complete pass lands them; a failed pass keeps the debt and says why.
    #[test]
    fn owed_code_links_are_disclosed_until_a_pass_lands_them() {
        let fx = two_widgets();
        assert_eq!(status(&fx.db)["reconciliation_pending"], 0);

        full_vault_refresh(&fx);
        mark_code_links_pending(&fx.db, "full vault refresh");
        let owed = status(&fx.db);
        assert_eq!(owed["reconciliation_pending"], 1, "{owed}");
        let reason = owed["reconciliation_pending_notes"][0]["reason"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(reason.contains("full vault refresh"), "{reason}");

        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(1));
        let failed = reconcile_code_links(&fx.store, &CrossDomainConfig::default());
        crate::cross_domain::FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(0));
        assert!(failed.is_err());
        let still = status(&fx.db);
        assert_eq!(still["reconciliation_pending"], 1, "{still}");
        let reason = still["reconciliation_pending_notes"][0]["reason"]
            .as_str()
            .unwrap();
        assert!(
            reason.contains("last attempt failed") && reason.contains("injected"),
            "{reason}"
        );

        reconcile(&fx.store);
        assert_eq!(status(&fx.db)["reconciliation_pending"], 0);
        assert_eq!(edges(&fx.store).len(), 4);
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
