//! Cross-domain link discovery: notes ↔ code.
//!
//! Finds where a note mentions an indexed code symbol and emits
//! `REFERENCES_CODE_*` edges. These edges are the architectural keystone of
//! the brain — once present, a single PPR run over the unified scope (which
//! includes them; see `GraphScope::unified`) ranks "Auth Service Design.md"
//! and `AuthService::authenticate` together for a query seeded with either.
//!
//! nw-670 (ADR "Note-to-code links only for real code mentions"): a note
//! links to a symbol when the author wrote the name AS CODE (or as an
//! unmistakable identifier) and the name resolves to that symbol within the
//! note's project. Ordinary English words never create links. Matching every
//! word against every symbol name produced ~39 M edges on a real brain at
//! 2.9 % precision, and the noise split each note's PPR mass so thinly that
//! the symbols it really named never ranked. The rules, as the ADR numbers
//! them:
//!
//! * R1/R2 — mentions ([`nestweaver_parser::code_mentions`]): a whole inline
//!   code span that is one identifier; elsewhere in code or prose, a
//!   DISTINCTIVE identifier (`snake_case`, `camelCase` with two humps) or an
//!   unqualified call `name(`. Never inside URLs, wikilinks or link targets,
//!   nor inside string literals in code.
//! * R3 — a PLAIN name resolves only from a whole code span, to a type or an
//!   all-caps constant, or from a call, to a function or method; never to a
//!   module or a value (`Property`, `Variable`, lowercase `Constant`).
//! * R4 — prefer non-test definitions, then definitions over values.
//! * R5 — within the repos of the note's projects; a note in no project links
//!   only distinctive names defined in exactly one file.
//! * R6 — at most [`MAX_DEFINING_FILES`] distinct defining files, else none.
//! * R7 — `main` joins the stoplist.
//! * R8 — each name resolves once per note; sections link it only where
//!   their own lines mention it.
//! * R9 — confidence = kind base × evidence (code 1.0, prose 0.85) ÷ the
//!   number of defining files linked, so an ambiguous name spends one unit
//!   of PPR mass in total.
//!
//! Names shorter than 4 characters and [`STOPLIST`] words are never
//! candidates. One implementation serves the bulk pass, the vault watcher
//! and the code-link reconciler ([`crate::code_links`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use nestweaver_parser::MentionFlags;
use nestweaver_schema::SymbolKind;
use nestweaver_store::GraphStore;

use crate::config::CrossDomainConfig;
use crate::content_reader::ContentReader;

/// Map from vault UID to a [`ContentReader`] that can read that vault's files.
///
/// In local (daemon) mode, vaults live on the filesystem and the cross-domain
/// scanner falls back to `std::fs::read_to_string` when no reader is provided.
/// In server mode with bare clones, the files do not exist at the vault's
/// `root_path`, so a [`crate::content_reader::GitBareReader`] must be supplied
/// here for each vault. Without it, all note reads fail silently and zero
/// Note-to-Symbol edges are built.
pub type VaultReaders<'a> = HashMap<String, &'a dyn ContentReader>;

/// Read a note's body content, preferring a [`ContentReader`] from
/// `vault_readers` when one is available for the note's vault, and falling back
/// to direct filesystem access otherwise (local/daemon mode).
fn read_note_body(
    store: &GraphStore,
    note: &nestweaver_schema::Note,
    vault_readers: &VaultReaders<'_>,
) -> Option<String> {
    // Try the ContentReader first (required for bare-clone / server mode).
    if let Some(reader) = vault_readers.get(&note.vault_uid) {
        return reader.read_file(Path::new(&note.file_path)).ok();
    }

    // Fallback: read from the filesystem using the vault's root_path.
    let vault = store.lookup_vault(&note.vault_uid).ok()?;
    let path = Path::new(&vault.root_path).join(&note.file_path);
    std::fs::read_to_string(&path).ok()
}

/// Outcome of a discovery pass — surfaces in CLI/MCP output so users
/// can see what happened.
#[derive(Debug, Clone, Default)]
pub struct CrossDomainResult {
    pub notes_scanned: usize,
    pub note_to_symbol_edges: usize,
    pub section_to_symbol_edges: usize,
    pub skipped_unreadable: usize,
}

/// Minimum symbol name length to consider for matching. Below this the
/// false-positive rate from collisions with English words is unworkable.
/// Tuned for typical OO/JS codebases; bump for projects with many
/// short identifiers.
const MIN_SYMBOL_NAME_LEN: usize = 4;

/// Common English words that also appear as identifier names in typical
/// codebases. Matching on these produces false-positive cross-domain
/// edges because they appear naturally in prose. All entries are
/// lowercase; matching is case-insensitive.
pub const STOPLIST: &[&str] = &[
    "error", "config", "state", "user", "result", "file", "path", "time", "date", "type", "value",
    "hash", "status", "event", "source", "data", "name", "code", "node", "list", "table", "view",
    "model", "item", "entry", "record", "field", "index", "query", "task", "test", "group",
    "block", "point", "range", "span", "token", "line", "rule", "step", "match", "link", "text",
    "body", "title", "header", "label", "option", "context", "handle", "client", "server",
    "service", "request", "response", "command", "action", "buffer", "stream", "reader", "writer",
    "parser", "builder", "filter", "logger", "target", "count", "total", "input", "output",
    "format", "cache", "store", "queue", "stack", "array", "batch", "page",
    // nw-670 R7: the git default branch and every entry point's name. 477
    // symbols on a real brain, and never the one a note meant.
    "main",
];

/// Discover and persist cross-domain links across the entire graph.
/// Designed to be called after both `index_directory` and
/// `index_markdown_directory` have populated the DB. Safe to re-run:
/// each note's existing REFERENCES_CODE edges are deleted before
/// re-emitting.
///
/// Falls back to `std::fs::read_to_string` for note content (local mode).
/// For server mode with bare clones, use
/// [`discover_cross_domain_links_with_readers`] instead.
pub fn discover_cross_domain_links(store: &GraphStore) -> Result<CrossDomainResult, anyhow::Error> {
    discover_cross_domain_links_with_config(store, &CrossDomainConfig::default())
}

/// Like [`discover_cross_domain_links`] but accepts [`VaultReaders`] so
/// note content can be read from bare clones in server mode.
pub fn discover_cross_domain_links_with_readers(
    store: &GraphStore,
    vault_readers: &VaultReaders<'_>,
) -> Result<CrossDomainResult, anyhow::Error> {
    discover_cross_domain_links_full(store, &CrossDomainConfig::default(), vault_readers)
}

/// [`discover_cross_domain_links_with_readers`] honouring the instance's
/// `[cross_domain]` settings (nw-673: the server worker's route).
pub fn discover_cross_domain_links_with_readers_and_config(
    store: &GraphStore,
    vault_readers: &VaultReaders<'_>,
    config: &CrossDomainConfig,
) -> Result<CrossDomainResult, anyhow::Error> {
    discover_cross_domain_links_full(store, config, vault_readers)
}

/// Like `discover_cross_domain_links` but honours the provided `CrossDomainConfig`.
pub fn discover_cross_domain_links_with_config(
    store: &GraphStore,
    config: &CrossDomainConfig,
) -> Result<CrossDomainResult, anyhow::Error> {
    discover_cross_domain_links_full(store, config, &VaultReaders::new())
}

/// nw-670: the version of the note→code link rules below. Bump it whenever
/// a rule change alters which edges a note should have: stored links built
/// by another version are replaced wholesale by the code-link reconciler
/// ([`crate::code_links`]), not left to linger until each note is edited.
///
/// 1 (implicit, absent) — every word of 4+ characters matched every symbol
///   of that name, any kind, any repo.
/// 2 — nw-670: explicit code mentions, kind-gated by shape, project-scoped,
///   at most [`MAX_DEFINING_FILES`] defining files, `main` stoplisted.
pub const CROSS_DOMAIN_RULES_VERSION: u32 = 2;

/// Notes flushed per write transaction, by the bulk pass and the vault
/// watcher alike. Bounds peak transaction memory while amortising the commit
/// (an fsync) over many notes. nw-668: the watcher also takes its write lease
/// once per chunk of this size, so a large replayed batch releases the gate
/// between chunks for FIFO-queued reconcilers.
pub(crate) const NOTES_PER_TXN: usize = 100;

/// What one cross-domain flush cost the store (nw-668): write transactions
/// opened and statements executed. The statement count is independent of how
/// many symbols a note mentions — the property that keeps a note with 100K
/// name matches from costing 100K commits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CrossDomainFlushStats {
    pub transactions: usize,
    pub statements: usize,
}

#[cfg(test)]
thread_local! {
    /// Test failpoint (nw-668): fail this many upcoming flushes after their
    /// deletes and note-level inserts ran and before their section-level
    /// inserts — the point where the old auto-commit path left a partial
    /// edge set.
    pub(crate) static FAIL_CROSS_DOMAIN_FLUSHES: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
}

/// Full implementation accepting both config and vault readers.
fn discover_cross_domain_links_full(
    store: &GraphStore,
    config: &CrossDomainConfig,
    vault_readers: &VaultReaders<'_>,
) -> Result<CrossDomainResult, anyhow::Error> {
    let index = build_symbol_index_with_config(store, config)?;
    if index.is_empty() {
        // No code indexed — nothing to bridge to. Not an error.
        return Ok(CrossDomainResult::default());
    }

    // A note the STORE could not decode is a note this pass did not scan, and
    // `skipped_unreadable` is already the field that says so — it just never
    // saw these, because nw-335's corrupt-row tolerance drops them inside the
    // scan and reports it in a log line. Left unwired, `notes_scanned` counts
    // them in neither column and the pass claims it covered the vault.
    let (notes, integrity) = store
        .list_notes_with_integrity(None)
        .context("list_notes")?;
    let mut result = CrossDomainResult::default();
    result.skipped_unreadable += integrity.skipped_corrupt;

    // Scan all notes in memory first (no DB writes), then flush in
    // transaction-batched chunks. Earlier versions committed once per
    // note × section, which on macOS amounts to thousands of fsync'd
    // commits and dominates indexing time.
    let mut pending: Vec<ScannedNote> = Vec::with_capacity(NOTES_PER_TXN);
    for note in &notes {
        match scan_one_note(store, note, &index, vault_readers)? {
            ScanOutcome::Scanned(scanned) => pending.push(scanned),
            ScanOutcome::Skipped => result.skipped_unreadable += 1,
        }
        if pending.len() >= NOTES_PER_TXN {
            flush_scanned_notes(store, &pending, &mut result)?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        flush_scanned_notes(store, &pending, &mut result)?;
    }

    Ok(result)
}

/// Accumulated scan results for a single note — built outside any
/// transaction so the heavy tokenising work runs lock-free, then
/// flushed in batched transactions by `flush_scanned_notes`.
pub(crate) struct ScannedNote {
    note_uid: String,
    note_edges: Vec<(String, String, f32, &'static str)>,
    section_edges: Vec<(String, String, f32, &'static str)>,
}

enum ScanOutcome {
    Scanned(ScannedNote),
    Skipped,
}

/// Flush a batch of scanned notes inside a single write transaction:
/// delete the notes' existing cross-domain edges, then insert the fresh
/// ones, with set-based statements (nw-668) — a constant number per edge
/// kind, not one per edge. One commit per batch, not per note × section ×
/// edge. Any failure rolls the transaction back, so every note in `batch`
/// keeps its PREVIOUS edges rather than being left half-deleted.
///
/// Store errors are folded INTO the message (`{op}: {cause}`) rather than
/// attached via `anyhow::Context`: callers log discovery failures with `{e}`
/// (Display of the outermost error only), so a plain context would reduce the
/// warning to a bare function name and hide the underlying cause — e.g.
/// `Cannot execute write operations in a read-only database!`, which is what
/// `brain add` hits because its discovery store is opened read-only.
pub(crate) fn flush_scanned_notes(
    store: &GraphStore,
    batch: &[ScannedNote],
    result: &mut CrossDomainResult,
) -> Result<CrossDomainFlushStats, anyhow::Error> {
    if batch.is_empty() {
        return Ok(CrossDomainFlushStats::default());
    }
    let conn = store
        .begin_transaction()
        .map_err(|e| anyhow::anyhow!("begin_transaction for cross-domain flush: {e}"))?;
    let statements = match flush_on(&conn, batch) {
        Ok(statements) => statements,
        Err(error) => {
            if let Err(rollback) = store.rollback_transaction(&conn) {
                tracing::warn!(%rollback, "cross-domain flush rollback failed");
            }
            return Err(error);
        }
    };
    store
        .commit_transaction(&conn)
        .map_err(|e| anyhow::anyhow!("commit_transaction for cross-domain flush: {e}"))?;
    for scanned in batch {
        result.notes_scanned += 1;
        result.note_to_symbol_edges += scanned.note_edges.len();
        result.section_to_symbol_edges += scanned.section_edges.len();
    }
    Ok(CrossDomainFlushStats {
        transactions: 1,
        statements,
    })
}

/// The statements of [`flush_scanned_notes`], on its open transaction.
/// Returns how many were executed.
fn flush_on(
    conn: &nestweaver_store::DbConnection<'_>,
    batch: &[ScannedNote],
) -> Result<usize, anyhow::Error> {
    let note_uids: Vec<&str> = batch.iter().map(|s| s.note_uid.as_str()).collect();
    let mut statements = GraphStore::delete_cross_domain_edges_for_notes_on(conn, &note_uids)
        .map_err(|e| anyhow::anyhow!("delete_cross_domain_edges_for_notes_on: {e}"))?;

    let note_edges: Vec<_> = batch
        .iter()
        .flat_map(|s| edge_refs(&s.note_edges))
        .collect();
    statements += GraphStore::batch_insert_note_to_symbol_edges_on(conn, &note_edges)
        .map_err(|e| anyhow::anyhow!("batch_insert_note_to_symbol_edges_on: {e}"))?;
    #[cfg(test)]
    if FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| {
        let armed = fail.get();
        fail.set(armed.saturating_sub(1));
        armed > 0
    }) {
        anyhow::bail!("injected cross-domain flush failure");
    }
    let section_edges: Vec<_> = batch
        .iter()
        .flat_map(|s| edge_refs(&s.section_edges))
        .collect();
    statements += GraphStore::batch_insert_section_to_symbol_edges_on(conn, &section_edges)
        .map_err(|e| anyhow::anyhow!("batch_insert_section_to_symbol_edges_on: {e}"))?;
    Ok(statements)
}

fn edge_refs<'a>(
    edges: &'a [(String, String, f32, &'static str)],
) -> impl Iterator<Item = (&'a str, &'a str, f32, &'static str)> {
    edges
        .iter()
        .map(|(from, to, conf, src)| (from.as_str(), to.as_str(), *conf, *src))
}

/// Build a SymbolIndex from the store's symbol list. Pre-build once per
/// batch to avoid redundant DB queries when processing multiple notes.
pub fn build_symbol_index(store: &GraphStore) -> Result<SymbolIndex, anyhow::Error> {
    build_symbol_index_with_config(store, &CrossDomainConfig::default())
}

/// Like `build_symbol_index` but honours the provided `CrossDomainConfig`.
///
/// nw-670: also loads project membership, so every route that resolves a
/// note's mentions (bulk, watcher, reconciler) scopes it the same way.
pub fn build_symbol_index_with_config(
    store: &GraphStore,
    config: &CrossDomainConfig,
) -> Result<SymbolIndex, anyhow::Error> {
    let symbols = store
        .list_symbols_for_linking()
        .context("list_symbols_for_linking")?;
    let (note_projects, project_repos) =
        store.project_link_scopes().context("project_link_scopes")?;
    // nw-670 re-review F1 / nw-678: a project's repos come from its durable
    // PROJECT_INCLUDES_REPO membership (`project_link_scopes`), not only the
    // per-symbol edges every re-index dropped — which left no note scoped on
    // a real brain. A project with notes that DECLARES repos (recorded by
    // `materialize_projects`) but has none is disclosed.
    let unscoped_projects = store
        .db_path()
        .map(|db_path| unscoped_projects(db_path, &note_projects, &project_repos))
        .unwrap_or_default();
    let mut index = SymbolIndex::build_with_config(&symbols, config)
        .with_project_scopes(&note_projects, &project_repos);
    index.unscoped_projects = unscoped_projects;
    Ok(index)
}

/// Projects that include notes and declare repos (per `materialize_projects`)
/// but have no member repo (nw-670 re-review F1 / nw-678).
pub(crate) fn unscoped_projects(
    db_path: &Path,
    note_projects: &[(String, String)],
    project_repos: &[(String, String)],
) -> Vec<String> {
    let with_notes: HashSet<&str> = note_projects.iter().map(|(_, p)| p.as_str()).collect();
    let with_repos: HashSet<&str> = project_repos.iter().map(|(p, _)| p.as_str()).collect();
    let extensions = crate::extensions::load_extensions(db_path);
    let mut out: Vec<String> = extensions
        .iter()
        .filter(|(project, properties)| {
            properties
                .get(crate::project::DECLARED_REPO_COUNT_KEY)
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
                > 0
                && with_notes.contains(project.as_str())
                && !with_repos.contains(project.as_str())
        })
        .map(|(project, _)| project.clone())
        .collect();
    out.sort();
    out
}

/// Read-only scan: load the note body, scan for symbol mentions, and
/// return the edges in memory. Used by the bulk discovery path so the
/// DB writes can be deferred into a batched transaction.
fn scan_one_note(
    store: &GraphStore,
    note: &nestweaver_schema::Note,
    index: &SymbolIndex,
    vault_readers: &VaultReaders<'_>,
) -> Result<ScanOutcome, anyhow::Error> {
    let body = match read_note_body(store, note, vault_readers) {
        Some(s) => s,
        None => return Ok(ScanOutcome::Skipped),
    };
    let sections = store.sections_in_note(&note.uid).unwrap_or_default();
    Ok(ScanOutcome::Scanned(scan_note_source(
        &note.uid, &body, &sections, index,
    )))
}

/// Pure scan of one note's full source text against `index` (nw-668): the
/// note's mentions resolved once, then attributed to the sections whose
/// line spans hold them. No I/O, so the vault watcher runs it with no write
/// lease held, over the exact text its batch parsed and committed.
///
/// `sections` must come from parsing `source`: their file-absolute
/// `start_line`/`end_line` spans index into its lines.
///
/// nw-675: this is [`note_mentions`] then [`resolve_note`] — the split the
/// code-link reconciler uses to cache a note's mentions across passes. One
/// implementation behind the bulk pass, the vault watcher and the
/// reconciler, so the three cannot drift apart.
pub(crate) fn scan_note_source(
    note_uid: &str,
    source: &str,
    sections: &[nestweaver_schema::Section],
    index: &SymbolIndex,
) -> ScannedNote {
    let spans: Vec<SectionSpan> = sections.iter().map(SectionSpan::of).collect();
    resolve_note(note_uid, &note_mentions(source), &spans, index)
}

/// A section's uid and its file-absolute, 1-based inclusive line span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SectionSpan {
    pub(crate) uid: String,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
}

impl SectionSpan {
    fn of(section: &nestweaver_schema::Section) -> Self {
        Self {
            uid: section.uid.clone(),
            start_line: section.start_line,
            end_line: section.end_line,
        }
    }

    fn contains(&self, line: u32) -> bool {
        self.start_line >= 1 && line >= self.start_line && line <= self.end_line
    }
}

/// One mentioned name: how it was written anywhere in the note (the union
/// resolution runs on, R8), and each line it occurs on with how it was
/// written there (section attribution and evidence, R8/R9).
#[derive(Debug, Clone)]
struct MentionedName {
    name: String,
    flags: MentionFlags,
    lines: Vec<(u32, MentionFlags)>,
}

/// What a note's text mentions as code, independent of any symbol index
/// (nw-670 R1, [`nestweaver_parser::code_mentions`]). nw-675: cheap to keep
/// per note, so the reconciler re-resolves a note against a changed symbol
/// index or project scope without re-reading or re-parsing it.
#[derive(Debug, Clone, Default)]
pub(crate) struct NoteMentions {
    names: Vec<MentionedName>,
}

/// Collect a note's mentions (see [`NoteMentions`]).
pub(crate) fn note_mentions(source: &str) -> NoteMentions {
    let mut by_name: HashMap<String, MentionedName> = HashMap::new();
    for mention in nestweaver_parser::code_mentions(source) {
        let entry = by_name
            .entry(mention.name.clone())
            .or_insert_with(|| MentionedName {
                name: mention.name.clone(),
                flags: MentionFlags::default(),
                lines: Vec::new(),
            });
        entry.flags.merge(mention.flags);
        match entry
            .lines
            .iter_mut()
            .find(|(line, _)| *line == mention.line)
        {
            Some((_, flags)) => flags.merge(mention.flags),
            None => entry.lines.push((mention.line, mention.flags)),
        }
    }
    let mut names: Vec<MentionedName> = by_name.into_values().collect();
    names.sort_by(|a, b| a.name.cmp(&b.name));
    NoteMentions { names }
}

/// R9 evidence: a name written as code counts in full; one seen only as
/// distinctive prose (or a call in prose) slightly less.
fn evidence(flags: MentionFlags) -> f32 {
    if flags.is_code() { 1.0 } else { 0.85 }
}

/// Resolve a note's mentions against `index` into its note-level edges and,
/// per section, the edges for names mentioned on that section's lines.
///
/// nw-670 R8: each name is resolved ONCE per note, on the union of how the
/// note writes it, so the note and its sections agree on what it means; a
/// section links a resolved symbol only where its own lines mention the
/// name, so section edges stay a subset of the note's.
pub(crate) fn resolve_note(
    note_uid: &str,
    mentions: &NoteMentions,
    sections: &[SectionSpan],
    index: &SymbolIndex,
) -> ScannedNote {
    let scope = index.scope_of(note_uid);
    let mut note_edges: Vec<(String, String, f32, &'static str)> = Vec::new();
    let mut section_edges: Vec<(String, String, f32, &'static str)> = Vec::new();
    let mut note_seen: HashSet<&str> = HashSet::new();
    let mut section_seen: HashSet<(&str, &str)> = HashSet::new();
    for mentioned in &mentions.names {
        let Some(resolution) = index.resolve(&mentioned.name, mentioned.flags, scope) else {
            continue;
        };
        let share = resolution.files as f32;
        let note_evidence = evidence(mentioned.flags);
        for candidate in &resolution.symbols {
            if note_seen.insert(candidate.uid.as_str()) {
                note_edges.push((
                    note_uid.to_string(),
                    candidate.uid.clone(),
                    candidate.kind_base * note_evidence / share,
                    "name-match",
                ));
            }
        }
        for section in sections {
            let mut here = MentionFlags::default();
            let mut present = false;
            for (line, flags) in &mentioned.lines {
                if section.contains(*line) {
                    here.merge(*flags);
                    present = true;
                }
            }
            if !present {
                continue;
            }
            for candidate in &resolution.symbols {
                if section_seen.insert((section.uid.as_str(), candidate.uid.as_str())) {
                    section_edges.push((
                        section.uid.clone(),
                        candidate.uid.clone(),
                        candidate.kind_base * evidence(here) / share,
                        "name-match",
                    ));
                }
            }
        }
    }
    ScannedNote {
        note_uid: note_uid.to_string(),
        note_edges,
        section_edges,
    }
}

impl ScannedNote {
    /// Edges this scan will write (note-level plus section-level).
    pub(crate) fn edge_count(&self) -> usize {
        self.note_edges.len() + self.section_edges.len()
    }

    /// The note this scan belongs to.
    pub(crate) fn note_uid(&self) -> &str {
        &self.note_uid
    }

    /// Every edge this scan would write, note- and section-level together,
    /// as `(from_uid, symbol_uid, confidence)`.
    pub(crate) fn edges(&self) -> impl Iterator<Item = (&str, &str, f32)> {
        self.note_edges
            .iter()
            .chain(&self.section_edges)
            .map(|(from, to, conf, _)| (from.as_str(), to.as_str(), *conf))
    }
}

/// nw-670 R6: a name links only when its candidates (after R3–R5) come from
/// at most this many distinct defining files; beyond that no single link is
/// honest and none is made. A product constant, not a knob: the ADR's
/// labelled evaluation puts the precision/recall knee here (K=5: 97.1 %
/// precision, 83.5 % recall; K=8 adds two names for 11 % more edges).
pub(crate) const MAX_DEFINING_FILES: usize = 5;

/// R5: a note outside every project links only names defined in exactly one
/// file across the instance — there is no project to disambiguate with.
const MAX_DEFINING_FILES_UNSCOPED: usize = 1;

/// One symbol a name may resolve to.
#[derive(Debug, Clone)]
struct Candidate {
    uid: String,
    kind: SymbolKind,
    repo_uid: Arc<str>,
    file_path: Arc<str>,
    /// Defined under a test path (R4 demotes it when a real one exists).
    in_test: bool,
    /// A definition rather than a value (R4 prefers definitions).
    definition: bool,
    /// R9 `kind_base`.
    kind_base: f32,
}

/// What a mentioned name resolved to: the symbols to link and how many
/// distinct files define them (R9 divides the confidence by it).
struct Resolution<'a> {
    symbols: Vec<&'a Candidate>,
    files: usize,
}

fn parse_kind(kind: &str) -> Option<SymbolKind> {
    Some(match kind {
        "Function" => SymbolKind::Function,
        "Class" => SymbolKind::Class,
        "Method" => SymbolKind::Method,
        "Interface" => SymbolKind::Interface,
        "Trait" => SymbolKind::Trait,
        "Enum" => SymbolKind::Enum,
        "Module" => SymbolKind::Module,
        "Extension" => SymbolKind::Extension,
        "Constant" => SymbolKind::Constant,
        "Property" => SymbolKind::Property,
        "TypeAlias" => SymbolKind::TypeAlias,
        "Variable" => SymbolKind::Variable,
        _ => return None,
    })
}

/// R3's type-like kinds: a plain name written as a whole code span may
/// resolve to these.
fn is_type_like(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::TypeAlias
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Extension
    )
}

/// `TIMEOUT`, `MAX_RETRIES`: an all-caps constant name (R3, R4).
fn is_screaming(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !name.ends_with('_')
        && !name.contains("__")
}

/// R9 `kind_base`.
fn kind_base(kind: SymbolKind) -> f32 {
    match kind {
        SymbolKind::Function => 0.9,
        SymbolKind::Method | SymbolKind::Module => 0.7,
        SymbolKind::Constant | SymbolKind::Property | SymbolKind::Variable => 0.6,
        _ => 0.8,
    }
}

/// R4: a definition in tests, fixtures or mocks, demoted when the name also
/// has a real definition (`tests/`, `__tests__/`, `spec/`, `e2e/`,
/// `fixtures/`, `__mocks__/`, `*.test.*`, `*_test.*`, `*-spec.*`,
/// `test_*.py`).
fn is_test_path(path: &str) -> bool {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let is_dir = parts.peek().is_some();
        if is_dir
            && matches!(
                part,
                "test"
                    | "tests"
                    | "__tests__"
                    | "spec"
                    | "e2e"
                    | "fixture"
                    | "fixtures"
                    | "__mocks__"
            )
        {
            return true;
        }
        if !is_dir {
            let Some((stem, _ext)) = part.rsplit_once('.') else {
                return false;
            };
            return ["test", "spec"].iter().any(|word| {
                [".", "_", "-"]
                    .iter()
                    .any(|sep| stem.ends_with(&format!("{sep}{word}")))
            }) || (part.starts_with("test_") && part.ends_with(".py"));
        }
    }
    false
}

/// Symbol name → its candidates, plus each note's project scope. Built once
/// per discovery pass, watcher batch or reconcile pass.
pub struct SymbolIndex {
    by_name: HashMap<String, Vec<Candidate>>,
    /// Note uid → index into `scopes`, for notes in a project with repos.
    note_scope: HashMap<String, usize>,
    /// Each distinct scope: the repo uids of a note's projects.
    scopes: Vec<HashSet<Arc<str>>>,
    /// nw-670 re-review F1: projects that include notes and DECLARE repos,
    /// none of which resolved — their notes link as if in no project. The
    /// code-link reconciler discloses them.
    pub(crate) unscoped_projects: Vec<String>,
}

impl SymbolIndex {
    fn build_with_config(
        symbols: &[(String, String, String, String, String)],
        config: &CrossDomainConfig,
    ) -> Self {
        // Compute effective stoplist: replace entirely or extend the built-in.
        let effective_stoplist: HashSet<String> = if let Some(replace) = &config.stoplist_replace {
            replace.iter().map(|s| s.to_ascii_lowercase()).collect()
        } else {
            STOPLIST
                .iter()
                .map(|s| (*s).to_string())
                .chain(
                    config
                        .stoplist_extend
                        .iter()
                        .map(|s| s.to_ascii_lowercase()),
                )
                .collect()
        };

        let min_len = config.min_symbol_name_length.unwrap_or(MIN_SYMBOL_NAME_LEN);

        let mut interned: HashSet<Arc<str>> = HashSet::new();
        let mut intern = |value: &str| -> Arc<str> {
            if let Some(existing) = interned.get(value) {
                return Arc::clone(existing);
            }
            let arc: Arc<str> = Arc::from(value);
            interned.insert(Arc::clone(&arc));
            arc
        };
        let mut by_name: HashMap<String, Vec<Candidate>> = HashMap::new();
        for (uid, name, kind, repo_uid, file_path) in symbols {
            if name.len() < min_len {
                continue;
            }
            if effective_stoplist.contains(name.to_ascii_lowercase().as_str()) {
                continue;
            }
            // Only accept valid identifier-shaped names — alphanumerics
            // and underscores. Drops parser-emitted oddities like
            // `<anonymous>` or scope-qualified `Foo::bar`.
            if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let Some(kind) = parse_kind(kind) else {
                continue;
            };
            let definition = matches!(
                kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Module
            ) || is_type_like(kind)
                || (kind == SymbolKind::Constant && is_screaming(name));
            by_name.entry(name.clone()).or_default().push(Candidate {
                uid: uid.clone(),
                kind,
                in_test: is_test_path(file_path),
                repo_uid: intern(repo_uid),
                file_path: intern(file_path),
                definition,
                kind_base: kind_base(kind),
            });
        }
        Self {
            by_name,
            note_scope: HashMap::new(),
            scopes: Vec::new(),
            unscoped_projects: Vec::new(),
        }
    }

    /// Attach project scopes (R5): a note's scope is the repos of every
    /// project that includes it. A note whose projects reach no repo (a
    /// notes-only project) is unscoped, like a note in no project.
    fn with_project_scopes(
        mut self,
        note_projects: &[(String, String)],
        project_repos: &[(String, String)],
    ) -> Self {
        let mut repos_of: HashMap<&str, Vec<Arc<str>>> = HashMap::new();
        for (project, repo) in project_repos {
            repos_of
                .entry(project.as_str())
                .or_default()
                .push(Arc::from(repo.as_str()));
        }
        let mut per_note: HashMap<&str, HashSet<Arc<str>>> = HashMap::new();
        for (note, project) in note_projects {
            let repos = per_note.entry(note.as_str()).or_default();
            if let Some(project_repos) = repos_of.get(project.as_str()) {
                repos.extend(project_repos.iter().cloned());
            }
        }
        let mut scope_ids: HashMap<Vec<Arc<str>>, usize> = HashMap::new();
        for (note, repos) in per_note {
            if repos.is_empty() {
                continue;
            }
            let mut key: Vec<Arc<str>> = repos.iter().cloned().collect();
            key.sort();
            let id = *scope_ids.entry(key).or_insert_with(|| {
                self.scopes.push(repos.clone());
                self.scopes.len() - 1
            });
            self.note_scope.insert(note.to_string(), id);
        }
        self
    }

    fn scope_of(&self, note_uid: &str) -> Option<&HashSet<Arc<str>>> {
        self.note_scope
            .get(note_uid)
            .and_then(|id| self.scopes.get(*id))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// nw-670 R3–R6: what `name`, written as `flags` describes, resolves to
    /// for a note in `scope` (`None`: in no project). `None` means no link.
    fn resolve(
        &self,
        name: &str,
        flags: MentionFlags,
        scope: Option<&HashSet<Arc<str>>>,
    ) -> Option<Resolution<'_>> {
        let distinctive = nestweaver_parser::is_distinctive(name);
        let mut candidates: Vec<&Candidate> = self.by_name.get(name)?.iter().collect();
        // R3: a plain name could be an English word. It resolves only when
        // written as a whole code span — to a type, or to an all-caps
        // constant — or as an unqualified call, to a function or method.
        // Never to a module or a value.
        if !distinctive {
            let all_caps = is_screaming(name);
            candidates.retain(|candidate| {
                (flags.span && is_type_like(candidate.kind))
                    || (flags.span && all_caps && candidate.kind == SymbolKind::Constant)
                    || (flags.call
                        && matches!(candidate.kind, SymbolKind::Function | SymbolKind::Method))
            });
        }
        // R4: go-to-definition's preferences — real code over tests, then
        // definitions over values.
        if candidates.iter().any(|candidate| !candidate.in_test) {
            candidates.retain(|candidate| !candidate.in_test);
        }
        if candidates.iter().any(|candidate| candidate.definition) {
            candidates.retain(|candidate| candidate.definition);
        }
        // R5: within the note's project scope, or — for a note in none — a
        // distinctive name only, defined in exactly one file.
        let cap = match scope {
            Some(repos) => {
                candidates.retain(|candidate| repos.contains(&candidate.repo_uid));
                MAX_DEFINING_FILES
            }
            None if distinctive => MAX_DEFINING_FILES_UNSCOPED,
            None => return None,
        };
        if candidates.is_empty() {
            return None;
        }
        // R6: an ambiguous name links nothing.
        let files: HashSet<(&str, &str)> = candidates
            .iter()
            .map(|candidate| (&*candidate.repo_uid, &*candidate.file_path))
            .collect();
        if files.len() > cap {
            return None;
        }
        Some(Resolution {
            symbols: candidates,
            files: files.len(),
        })
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{
        Note, NoteKind, Symbol, SymbolKind, Vault, Visibility, repo_uid, symbol_uid, vault_uid,
    };
    use tempfile::tempdir;

    /// A symbol row as the store lists it for linking.
    fn sym(uid: &str, name: &str, kind: &str, repo: &str, file: &str) -> SymbolRow {
        (
            uid.to_string(),
            name.to_string(),
            kind.to_string(),
            repo.to_string(),
            file.to_string(),
        )
    }

    type SymbolRow = (String, String, String, String, String);

    fn index_of(symbols: &[SymbolRow]) -> SymbolIndex {
        SymbolIndex::build_with_config(symbols, &CrossDomainConfig::default())
    }

    /// `index_of`, with note `note:p` in a project over `repos`.
    fn scoped_index_of(symbols: &[SymbolRow], repos: &[&str]) -> SymbolIndex {
        let projects: Vec<(String, String)> = repos
            .iter()
            .map(|repo| ("proj:p".to_string(), repo.to_string()))
            .collect();
        index_of(symbols)
            .with_project_scopes(&[("note:p".to_string(), "proj:p".to_string())], &projects)
    }

    /// Note-level `(symbol_uid, confidence)` hits for `text` in note `note`.
    fn hits_in(index: &SymbolIndex, note: &str, text: &str) -> Vec<(String, f32)> {
        let mut hits: Vec<(String, f32)> = resolve_note(note, &note_mentions(text), &[], index)
            .note_edges
            .into_iter()
            .map(|(_, uid, conf, _)| (uid, conf))
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits
    }

    /// Hits for an unscoped note (one in no project).
    fn scan(index: &SymbolIndex, text: &str) -> Vec<(String, f32)> {
        hits_in(index, "note:t", text)
    }

    /// Hits for a note in a project over repo `r` — the setting where a
    /// PLAIN name can link at all (R5).
    fn scan_in_project(symbols: &[SymbolRow], text: &str) -> Vec<(String, f32)> {
        hits_in(&scoped_index_of(symbols, &["r"]), "note:p", text)
    }

    fn uids(hits: &[(String, f32)]) -> Vec<&str> {
        hits.iter().map(|(uid, _)| uid.as_str()).collect()
    }

    /// nw-670: `Processor` is a plain word, so it links only when written as
    /// a whole code span (it used to link from bare prose). `Get` is below
    /// the minimum length and never links.
    #[test]
    fn symbol_index_skips_short_names() {
        let symbols = [
            sym("sym:1", "Get", "Function", "r", "src/a.rs"),
            sym("sym:2", "Processor", "Class", "r", "src/b.rs"),
        ];
        assert_eq!(
            uids(&scan_in_project(&symbols, "calls `Get()` and `Processor`")),
            ["sym:2"]
        );
        assert!(scan_in_project(&symbols, "`Get`").is_empty());
    }

    #[test]
    fn symbol_index_dedupes_within_a_text() {
        let idx = index_of(&[sym("sym:x", "AuthenticatorService", "Class", "r", "a.rs")]);
        let hits = scan(
            &idx,
            "AuthenticatorService and `AuthenticatorService` and AuthenticatorService",
        );
        assert_eq!(hits.len(), 1);
    }

    /// R1/R2: prose plain words are not mentions — `screen` and `before`
    /// used to link to every local const and function of those names.
    /// Counterweight: a distinctive name in the same prose links.
    #[test]
    fn plain_prose_words_do_not_link_but_distinctive_ones_do() {
        let idx = index_of(&[
            sym("sym:screen", "screen", "Constant", "r", "src/a.ts"),
            sym("sym:before", "before", "Function", "r", "src/b.ts"),
            sym("sym:sr", "ScreenRenderer", "Class", "r", "src/c.ts"),
        ]);
        assert!(scan(&idx, "the screen renders before layout").is_empty());
        assert_eq!(
            uids(&scan(&idx, "the screen renders before ScreenRenderer runs")),
            ["sym:sr"]
        );
    }

    /// R1: URLs, wikilink targets and markdown link targets are never
    /// mentions. Counterweight: text beside them still is.
    #[test]
    fn urls_wikilinks_and_link_targets_are_not_mentions() {
        let idx = index_of(&[
            sym("sym:pr", "pull_request_id", "Function", "r", "src/a.rs"),
            sym("sym:wl", "note_about_things", "Function", "r", "src/b.rs"),
            sym("sym:lt", "main_module", "Module", "r", "src/c.rs"),
            sym("sym:ok", "other_widget", "Function", "r", "src/d.rs"),
        ]);
        let text = "see https://x.io/pull_request_id and [[note_about_things]] and \
                    [the doc](src/main_module.rs) then other_widget runs";
        assert_eq!(uids(&scan(&idx, text)), ["sym:ok"]);
    }

    /// R1: string literals inside code are data, not code. Counterweight: a
    /// call in the same kind of span links.
    #[test]
    fn string_literals_in_code_are_not_mentions() {
        let idx = index_of(&[
            sym("sym:cs", "cleared_state", "Constant", "r", "src/a.ts"),
            sym("sym:rc", "readCsvRows", "Function", "r", "src/b.ts"),
        ]);
        assert!(scan(&idx, "set `status = 'cleared_state'` first").is_empty());
        assert_eq!(uids(&scan(&idx, "then `readCsvRows(x)`")), ["sym:rc"]);
    }

    /// R1: a qualified call of a plain name (`Math.round(`) is someone
    /// else's method. Counterweight: the unqualified call links.
    #[test]
    fn a_qualified_plain_call_does_not_link_but_an_unqualified_one_does() {
        let symbols = [sym("sym:round", "round", "Function", "r", "src/m.ts")];
        assert!(scan_in_project(&symbols, "uses `Math.round(x)`").is_empty());
        assert_eq!(
            uids(&scan_in_project(&symbols, "uses `round(x)`")),
            ["sym:round"]
        );
    }

    /// R3: a plain name never resolves to a value kind — `pending` is a
    /// status string far more often than a constant. Counterweight: a
    /// distinctive value name links.
    #[test]
    fn a_plain_span_never_resolves_to_a_value() {
        let symbols = [
            sym("sym:p", "pending", "Constant", "r", "src/a.ts"),
            sym("sym:pc", "pending_count", "Property", "r", "src/b.ts"),
        ];
        assert!(scan_in_project(&symbols, "status is `pending`").is_empty());
        assert_eq!(
            uids(&scan_in_project(&symbols, "see `pending_count`")),
            ["sym:pc"]
        );
    }

    /// R3: a plain whole span resolves to a type, not a function; a
    /// function needs the call shape.
    #[test]
    fn a_plain_span_resolves_to_types_and_a_call_to_functions() {
        let symbols = [
            sym("sym:d", "Dashboard", "Class", "r", "src/a.tsx"),
            sym("sym:r", "review", "Function", "r", "src/b.ts"),
        ];
        assert_eq!(
            uids(&scan_in_project(&symbols, "the `Dashboard` view")),
            ["sym:d"]
        );
        assert!(scan_in_project(&symbols, "needs `review`").is_empty());
        assert_eq!(
            uids(&scan_in_project(&symbols, "call `review()`")),
            ["sym:r"]
        );
    }

    /// R3: a plain name never resolves to a module — `watcher` would hit a
    /// Rust file module. Counterweight: a distinctive module name links.
    #[test]
    fn a_plain_span_never_resolves_to_a_module() {
        let symbols = [
            sym("sym:w", "watcher", "Module", "r", "src/watcher.rs"),
            sym(
                "sym:wc",
                "watcher_config",
                "Module",
                "r",
                "src/watcher_config.rs",
            ),
        ];
        assert!(scan_in_project(&symbols, "the `watcher`").is_empty());
        assert_eq!(
            uids(&scan_in_project(&symbols, "the `watcher_config`")),
            ["sym:wc"]
        );
    }

    /// R3: an all-caps constant links from a whole span. Counterweight: the
    /// same word in prose is a shout, not code.
    #[test]
    fn an_all_caps_span_resolves_to_its_constant_but_prose_does_not() {
        let symbols = [sym("sym:p", "PASSES", "Constant", "r", "src/a.rs")];
        assert_eq!(uids(&scan_in_project(&symbols, "see `PASSES`")), ["sym:p"]);
        assert!(scan_in_project(&symbols, "it PASSES now").is_empty());
    }

    /// R4: a real definition beats a test one. Counterweight: a name
    /// defined only in tests still links.
    #[test]
    fn test_definitions_give_way_to_real_ones() {
        let idx = index_of(&[
            sym("sym:real", "parseLedger", "Function", "r", "src/a.ts"),
            sym(
                "sym:test",
                "parseLedger",
                "Function",
                "r",
                "src/__tests__/a.test.ts",
            ),
            sym(
                "sym:only",
                "fixtureLedger",
                "Function",
                "r",
                "tests/fixtures.ts",
            ),
        ]);
        assert_eq!(uids(&scan(&idx, "parseLedger")), ["sym:real"]);
        assert_eq!(uids(&scan(&idx, "fixtureLedger")), ["sym:only"]);
    }

    /// R4: a definition beats same-named values. Counterweight: a name with
    /// only values links them.
    #[test]
    fn definitions_win_over_same_named_values() {
        let mut symbols = vec![sym("sym:f", "accountId", "Function", "r", "src/f.ts")];
        for i in 0..4 {
            symbols.push(sym(
                &format!("sym:p{i}"),
                "accountId",
                "Property",
                "r",
                "src/f.ts",
            ));
            symbols.push(sym(
                &format!("sym:q{i}"),
                "ledgerRowId",
                "Property",
                "r",
                "src/g.ts",
            ));
        }
        let idx = index_of(&symbols);
        assert_eq!(uids(&scan(&idx, "accountId")), ["sym:f"]);
        assert_eq!(
            uids(&scan(&idx, "ledgerRowId")),
            ["sym:q0", "sym:q1", "sym:q2", "sym:q3"]
        );
    }

    /// R5: a note in a project resolves only within its repos; no guessing
    /// into another project. Counterweight: the same name defined in the
    /// project's repo links.
    #[test]
    fn a_project_note_links_only_within_its_repos() {
        let outside = scoped_index_of(
            &[sym(
                "sym:x",
                "requireWrite",
                "Function",
                "repo:b",
                "src/a.ts",
            )],
            &["repo:a"],
        );
        assert!(hits_in(&outside, "note:p", "requireWrite").is_empty());
        let inside = scoped_index_of(
            &[
                sym("sym:x", "requireWrite", "Function", "repo:b", "src/a.ts"),
                sym("sym:y", "requireWrite", "Function", "repo:a", "src/a.ts"),
            ],
            &["repo:a"],
        );
        assert_eq!(uids(&hits_in(&inside, "note:p", "requireWrite")), ["sym:y"]);
    }

    /// R5: a note in no project links only a distinctive name with exactly
    /// one defining file. Counterweight: a unique one links.
    #[test]
    fn an_unscoped_note_links_only_unique_distinctive_names() {
        let idx = index_of(&[
            sym("sym:a", "writeBatch", "Function", "r1", "src/a.ts"),
            sym("sym:b", "writeBatch", "Function", "r2", "src/b.ts"),
            sym("sym:c", "claimLedger", "Function", "r1", "src/c.ts"),
            sym("sym:d", "Dashboard", "Class", "r1", "src/d.tsx"),
        ]);
        assert!(scan(&idx, "writeBatch").is_empty());
        assert_eq!(uids(&scan(&idx, "claimLedger")), ["sym:c"]);
        assert!(
            scan(&idx, "the `Dashboard`").is_empty(),
            "a plain name needs a project to disambiguate it"
        );
        let scoped = scoped_index_of(
            &[sym("sym:d", "Dashboard", "Class", "r1", "src/d.tsx")],
            &["r1"],
        );
        assert_eq!(
            uids(&hits_in(&scoped, "note:p", "the `Dashboard`")),
            ["sym:d"]
        );
    }

    /// R6: more than 5 defining files is ambiguous and links nothing.
    /// Counterweight: exactly 5 link all of them. Literal counts, not the
    /// constant: K is product behaviour (the ADR's measured knee), so a
    /// change to it must fail here.
    #[test]
    fn more_defining_files_than_the_cap_link_nothing() {
        let files = |n: usize| -> Vec<SymbolRow> {
            (0..n)
                .map(|i| {
                    sym(
                        &format!("sym:{i}"),
                        "orgScope",
                        "Function",
                        "r",
                        &format!("src/{i}.ts"),
                    )
                })
                .collect()
        };
        let over = scoped_index_of(&files(6), &["r"]);
        assert!(hits_in(&over, "note:p", "orgScope").is_empty());
        let at = scoped_index_of(&files(5), &["r"]);
        assert_eq!(hits_in(&at, "note:p", "orgScope").len(), 5);
    }

    /// R7: `main` is stoplisted. Counterweight: `main_loop` is not.
    #[test]
    fn main_is_stoplisted() {
        let idx = scoped_index_of(
            &[
                sym("sym:m", "main", "Function", "r", "src/main.rs"),
                sym("sym:ml", "main_loop", "Function", "r", "src/lp.rs"),
            ],
            &["r"],
        );
        assert!(hits_in(&idx, "note:p", "`main()` on main").is_empty());
        assert_eq!(uids(&hits_in(&idx, "note:p", "`main_loop`")), ["sym:ml"]);
    }

    /// R8: a section links a resolved symbol only where its own lines
    /// mention the name; the note links it once; section edges are a subset
    /// of the note's.
    #[test]
    fn sections_link_only_the_names_their_own_lines_mention() {
        let idx = index_of(&[
            sym("sym:a", "AlphaWidget", "Class", "r", "src/a.rs"),
            sym("sym:b", "BravoWidget", "Class", "r", "src/b.rs"),
        ]);
        let text = "# One\n\nuses AlphaWidget\n\n# Two\n\nuses BravoWidget and `AlphaWidget`\n\n# Three\n\nnothing\n";
        let spans = [
            SectionSpan {
                uid: "s1".into(),
                start_line: 2,
                end_line: 4,
            },
            SectionSpan {
                uid: "s2".into(),
                start_line: 6,
                end_line: 8,
            },
            SectionSpan {
                uid: "s3".into(),
                start_line: 10,
                end_line: 11,
            },
        ];
        let scanned = resolve_note("note:t", &note_mentions(text), &spans, &idx);
        let mut sections: Vec<(&str, &str)> = scanned
            .section_edges
            .iter()
            .map(|(from, to, _, _)| (from.as_str(), to.as_str()))
            .collect();
        sections.sort();
        assert_eq!(
            sections,
            [("s1", "sym:a"), ("s2", "sym:a"), ("s2", "sym:b")]
        );
        let note: HashSet<&str> = scanned.note_edges.iter().map(|e| e.1.as_str()).collect();
        assert_eq!(note.len(), 2);
        assert!(sections.iter().all(|(_, to)| note.contains(to)));
    }

    /// R9: kind base × evidence ÷ defining files.
    #[test]
    fn confidence_is_kind_times_evidence_over_defining_files() {
        let conf = |hits: Vec<(String, f32)>| -> Vec<f32> {
            hits.into_iter()
                .map(|(_, conf)| (conf * 1000.0).round() / 1000.0)
                .collect()
        };
        let two = scoped_index_of(
            &[
                sym("sym:a", "syncLedger", "Function", "r", "src/a.ts"),
                sym("sym:b", "syncLedger", "Function", "r", "src/b.ts"),
            ],
            &["r"],
        );
        assert_eq!(conf(hits_in(&two, "note:p", "`syncLedger`")), [0.45, 0.45]);
        let one = index_of(&[sym("sym:a", "syncLedger", "Function", "r", "src/a.ts")]);
        assert_eq!(conf(scan(&one, "`syncLedger`")), [0.9]);
        assert_eq!(conf(scan(&one, "syncLedger")), [0.765]);
    }

    #[test]
    fn end_to_end_discovers_link_from_note_body_to_symbol() {
        // Set up a vault on disk with a note that mentions a known symbol.
        let dir = tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        let note_path = vault_root.join("design.md");
        std::fs::write(
            &note_path,
            "# Auth Design\n\nThe AuthService.authenticate flow handles login.\n",
        )
        .unwrap();

        // Build the store with a Vault + Note + Symbol that should match.
        let store = GraphStore::in_memory().unwrap();
        let v_uid = vault_uid("default", &vault_root.to_string_lossy());
        store
            .insert_vault(&Vault {
                uid: v_uid.clone(),
                name: "v".to_string(),
                root_path: vault_root.to_string_lossy().into_owned(),
                instance_id: "default".to_string(),
            })
            .unwrap();

        let n_uid = format!("note:{v_uid}:abc");
        store
            .insert_note(&Note {
                uid: n_uid.clone(),
                vault_uid: v_uid,
                file_path: "design.md".to_string(),
                title: "Auth Design".to_string(),
                note_kind: NoteKind::Design,
                word_count: 10,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        // Symbol called "AuthService" — class kind → confidence 0.8.
        let r_uid = repo_uid("default", "https://example.com/r");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: r_uid.clone(),
                url: "https://example.com/r".to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        let s_uid = symbol_uid(&r_uid, "src/auth.ts", "AuthService", 1);
        store
            .insert_symbol(&Symbol {
                uid: s_uid.clone(),
                name: "AuthService".to_string(),
                kind: SymbolKind::Class,
                repo_uid: r_uid,
                file_path: "src/auth.ts".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "class AuthService".to_string(),
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

        let result = discover_cross_domain_links(&store).unwrap();
        assert_eq!(result.notes_scanned, 1);
        assert!(
            result.note_to_symbol_edges >= 1,
            "expected at least one note→symbol edge, got {}",
            result.note_to_symbol_edges
        );

        let count = store.count_references_code_edges().unwrap();
        assert!(count >= 1, "edges should be persisted");
    }

    /// This pass reports `notes_scanned` and already carries
    /// `skipped_unreadable` for a note it could not read. nw-335 made the note
    /// scan itself drop a row it cannot decode and say so only in a log line,
    /// which put such a note in NEITHER column — the pass then reported having
    /// covered the vault when it had not. The two counters must still account
    /// for every note the vault holds.
    #[test]
    fn a_note_the_store_cannot_decode_is_counted_as_skipped_not_as_absent() {
        let dir = tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(
            vault_root.join("design.md"),
            "# Auth Design\n\nThe AuthService.authenticate flow handles login.\n",
        )
        .unwrap();

        let store = GraphStore::in_memory().unwrap();
        let v_uid = vault_uid("default", &vault_root.to_string_lossy());
        store
            .insert_vault(&Vault {
                uid: v_uid.clone(),
                name: "v".to_string(),
                root_path: vault_root.to_string_lossy().into_owned(),
                instance_id: "default".to_string(),
            })
            .unwrap();

        let note = |uid: &str, title: &str, file: &str| Note {
            uid: uid.to_string(),
            vault_uid: v_uid.clone(),
            file_path: file.to_string(),
            title: title.to_string(),
            note_kind: NoteKind::Design,
            word_count: 10,
            content_hash: "h".to_string(),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        };
        store
            .insert_note(&note(
                &format!("note:{v_uid}:ok"),
                "Auth Design",
                "design.md",
            ))
            .unwrap();
        // A NUL in a note the store WROTE is the LadybugDB #678 pattern the
        // canary exists to catch; the scan drops this row.
        store
            .insert_note(&note(
                &format!("note:{v_uid}:bad"),
                "Poi\u{0}soned",
                "poisoned.md",
            ))
            .unwrap();

        let r_uid = repo_uid("default", "https://example.com/r");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: r_uid.clone(),
                url: "https://example.com/r".to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: symbol_uid(&r_uid, "src/auth.ts", "AuthService", 1),
                name: "AuthService".to_string(),
                kind: SymbolKind::Class,
                repo_uid: r_uid,
                file_path: "src/auth.ts".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "class AuthService".to_string(),
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

        let result = discover_cross_domain_links(&store).unwrap();
        assert_eq!(
            result.skipped_unreadable, 1,
            "a note the STORE could not decode is a note this pass did not \
             scan, and `skipped_unreadable` is the field that says so: {result:?}"
        );
        assert_eq!(
            result.notes_scanned, 1,
            "the undecodable note must NOT be counted as scanned: {result:?}"
        );
    }

    #[test]
    fn discovery_on_read_only_store_fails_with_actionable_error() {
        // `index --repo` then `brain add` on the same DB
        // warned `cross-domain discovery failed:
        // delete_cross_domain_edges_for_note_on` and produced zero
        // REFERENCES_CODE edges. Root cause: `brain add` opens the discovery
        // store READ-ONLY (main.rs `open_store`), so every flush write fails
        // with "Cannot execute write operations in a read-only database!" —
        // but the WARN logged only the outermost context (a bare function
        // name), hiding that cause in the source chain. The flush path now
        // folds the store error into the top-level message so `{e}` logging
        // stays actionable.
        let dir = tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(
            vault_root.join("refunds.md"),
            "# Refund Design\n\nThe processRefund function calls applyCredit.\n",
        )
        .unwrap();
        let db_path = dir.path().join("test.lbug");

        crate::index_md::index_markdown_directory(&vault_root, &db_path, "default", "vault")
            .unwrap();

        let store = GraphStore::open_or_create(&db_path).unwrap();
        let r_uid = repo_uid("default", "https://example.com/r");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: r_uid.clone(),
                url: "https://example.com/r".to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: symbol_uid(&r_uid, "src/refund.js", "processRefund", 1),
                name: "processRefund".to_string(),
                kind: SymbolKind::Function,
                repo_uid: r_uid,
                file_path: "src/refund.js".to_string(),
                start_line: 1,
                end_line: 3,
                signature: "function processRefund()".to_string(),
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
        drop(store);

        // Reopen READ-ONLY, as `brain add`'s open_store does before discovery.
        let read_only = GraphStore::open_read_only(&db_path).unwrap();
        let err = discover_cross_domain_links(&read_only)
            .expect_err("discovery against a read-only store must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("delete_cross_domain_edges_for_notes_on"),
            "the failing operation must be named, got: {msg}"
        );
        assert!(
            msg.contains("read-only"),
            "the underlying cause must be visible in the Display output \
             (callers log with `{{e}}`, not `{{e:#}}`), got: {msg}"
        );
    }

    #[test]
    fn end_to_end_discovers_link_with_file_backed_store() {
        // Companion coverage: the batched-transaction flush path
        // must work against an on-disk (file-backed) database, not just the
        // in-memory store the tests above use. (The `brain add` failure that
        // prompted this turned out to be a read-only store — see
        // discovery_on_read_only_store_fails_with_actionable_error — but the
        // file-backed happy path had no coverage either.)
        let dir = tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(
            vault_root.join("design.md"),
            "# Auth Design\n\nThe AuthService.authenticate flow handles login.\n",
        )
        .unwrap();

        let store = GraphStore::open_or_create(&dir.path().join("test.lbug")).unwrap();

        let v_uid = vault_uid("default", &vault_root.to_string_lossy());
        store
            .insert_vault(&Vault {
                uid: v_uid.clone(),
                name: "v".to_string(),
                root_path: vault_root.to_string_lossy().into_owned(),
                instance_id: "default".to_string(),
            })
            .unwrap();

        store
            .insert_note(&Note {
                uid: format!("note:{v_uid}:abc"),
                vault_uid: v_uid,
                file_path: "design.md".to_string(),
                title: "Auth Design".to_string(),
                note_kind: NoteKind::Design,
                word_count: 10,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        let r_uid = repo_uid("default", "https://example.com/r");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: r_uid.clone(),
                url: "https://example.com/r".to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: symbol_uid(&r_uid, "src/auth.ts", "AuthService", 1),
                name: "AuthService".to_string(),
                kind: SymbolKind::Class,
                repo_uid: r_uid,
                file_path: "src/auth.ts".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "class AuthService".to_string(),
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

        let result = discover_cross_domain_links(&store)
            .expect("cross-domain discovery must succeed on a file-backed store");
        assert!(
            result.note_to_symbol_edges >= 1,
            "expected at least one note→symbol edge, got {}",
            result.note_to_symbol_edges
        );
        let count = store.count_references_code_edges().unwrap();
        assert!(count >= 1, "edges should be persisted");
    }

    #[test]
    fn symbol_index_skips_stoplist_words() {
        let idx = index_of(&[
            sym("sym:1", "Error", "Class", "r", "src/a.rs"),
            sym("sym:2", "AuthService", "Class", "r", "src/b.rs"),
        ]);
        let hits = scan(&idx, "`Error` and AuthService");
        assert_eq!(uids(&hits), ["sym:2"], "only AuthService should match");
    }

    #[test]
    fn stoplist_is_case_insensitive() {
        for name in ["error", "ERROR", "Error", "eRrOr"] {
            let idx = index_of(&[sym("sym:1", name, "Class", "r", "src/a.rs")]);
            assert!(idx.is_empty(), "'{name}' should be stopped");
        }
    }

    /// nw-673: the configured stoplist reaches bulk discovery.
    /// Counterweight: the default config links the same note.
    #[test]
    fn the_configured_stoplist_reaches_bulk_discovery() {
        let dir = tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "# A\n\nuses AlphaWidget\n").unwrap();
        let db_path = dir.path().join("test.lbug");
        crate::index_md::index_markdown_directory(&vault_root, &db_path, "default", "vault")
            .unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let r_uid = repo_uid("default", "https://example.com/r");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: r_uid.clone(),
                url: "https://example.com/r".to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: symbol_uid(&r_uid, "src/w.rs", "AlphaWidget", 1),
                name: "AlphaWidget".to_string(),
                kind: SymbolKind::Class,
                repo_uid: r_uid,
                file_path: "src/w.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "struct AlphaWidget".to_string(),
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
        let config = CrossDomainConfig {
            stoplist_extend: vec!["AlphaWidget".to_string()],
            ..Default::default()
        };
        discover_cross_domain_links_with_config(&store, &config).unwrap();
        assert_eq!(store.count_references_code_edges().unwrap(), 0);
        discover_cross_domain_links(&store).unwrap();
        assert_eq!(store.count_references_code_edges().unwrap(), 2);
    }

    #[test]
    fn discover_cross_domain_links_idempotent() {
        let dir = tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(
            vault_root.join("design.md"),
            "# Auth\n\nThe AuthService handles login.\n",
        )
        .unwrap();

        let store = GraphStore::in_memory().unwrap();
        let v_uid = vault_uid("default", &vault_root.to_string_lossy());
        store
            .insert_vault(&Vault {
                uid: v_uid.clone(),
                name: "v".to_string(),
                root_path: vault_root.to_string_lossy().into_owned(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:1".to_string(),
                vault_uid: v_uid,
                file_path: "design.md".to_string(),
                title: "Auth".to_string(),
                note_kind: NoteKind::Design,
                word_count: 5,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        let r_uid = repo_uid("default", "https://example.com/r");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: r_uid.clone(),
                url: "https://example.com/r".to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        let s_uid = symbol_uid(&r_uid, "src/auth.ts", "AuthService", 1);
        store
            .insert_symbol(&Symbol {
                uid: s_uid,
                name: "AuthService".to_string(),
                kind: SymbolKind::Class,
                repo_uid: r_uid,
                file_path: "src/auth.ts".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "class AuthService".to_string(),
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

        let r1 = discover_cross_domain_links(&store).unwrap();
        let count1 = store.count_references_code_edges().unwrap();

        let r2 = discover_cross_domain_links(&store).unwrap();
        let count2 = store.count_references_code_edges().unwrap();

        assert_eq!(
            count1, count2,
            "running discovery twice should not double edges (got {count1} then {count2})"
        );
        assert_eq!(r1.note_to_symbol_edges, r2.note_to_symbol_edges);
    }

    #[test]
    fn no_symbols_in_db_is_clean_noop() {
        let store = GraphStore::in_memory().unwrap();
        let result = discover_cross_domain_links(&store).unwrap();
        assert_eq!(result.notes_scanned, 0);
        assert_eq!(result.note_to_symbol_edges, 0);
    }

    /// One note through the production scan + transactional flush — the two
    /// steps the bulk pass and the vault watcher run. (The per-note public
    /// wrappers were removed once nw-668 left only tests calling them.)
    fn refresh_one_note(store: &GraphStore, note_uid: &str) -> anyhow::Result<(usize, usize)> {
        let index = build_symbol_index(store)?;
        let note = store.lookup_note(note_uid)?;
        match scan_one_note(store, &note, &index, &VaultReaders::new())? {
            ScanOutcome::Scanned(scanned) => {
                let mut result = CrossDomainResult::default();
                flush_scanned_notes(store, std::slice::from_ref(&scanned), &mut result)?;
                Ok((result.note_to_symbol_edges, result.section_to_symbol_edges))
            }
            ScanOutcome::Skipped => Ok((0, 0)),
        }
    }

    /// nw-668: a single-note refresh deleted the note's edges in its own
    /// auto-committed statements and then inserted, so a failure between the
    /// two committed a partial set. It is one transaction now: a failure
    /// rolls back to the note's PREVIOUS edges. (Unlike the vault watcher,
    /// this path does not recreate the note's nodes first, so its previous
    /// edges are still there to keep.)
    #[test]
    fn a_failed_single_note_flush_keeps_the_notes_previous_edges() {
        let dir = tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "# A\n\nuses AlphaWidget\n").unwrap();
        let db_path = dir.path().join("test.lbug");
        crate::index_md::index_markdown_directory(&vault_root, &db_path, "default", "vault")
            .unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let r_uid = repo_uid("default", "https://example.com/r");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: r_uid.clone(),
                url: "https://example.com/r".to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "default".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        for (line, name) in ["AlphaWidget", "BravoWidget"].into_iter().enumerate() {
            store
                .insert_symbol(&Symbol {
                    uid: symbol_uid(&r_uid, "src/w.rs", name, line as u32 + 1),
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    repo_uid: r_uid.clone(),
                    file_path: "src/w.rs".to_string(),
                    start_line: line as u32 + 1,
                    end_line: line as u32 + 1,
                    signature: format!("fn {name}()"),
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
        }
        discover_cross_domain_links(&store).unwrap();
        let previous = store.list_references_code_edges().unwrap();
        assert_eq!(previous.len(), 2, "precondition: note + section edge");
        let note = store.list_notes(None).unwrap().remove(0);

        std::fs::write(
            vault_root.join("a.md"),
            "# A\n\nuses AlphaWidget and BravoWidget\n",
        )
        .unwrap();
        FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(1));
        let failed = refresh_one_note(&store, &note.uid);
        FAIL_CROSS_DOMAIN_FLUSHES.with(|fail| fail.set(0));
        assert!(failed.is_err(), "the injected failure must surface");
        assert_eq!(
            store.list_references_code_edges().unwrap(),
            previous,
            "a failed flush must roll back to the previous edges, not commit a partial set"
        );

        // Counterweight: the same refresh without the failure replaces them.
        assert_eq!(refresh_one_note(&store, &note.uid).unwrap(), (2, 2));
        assert_eq!(store.list_references_code_edges().unwrap().len(), 4);
    }
}
