//! Cross-domain link discovery: notes ↔ code.
//!
//! Scans note bodies for occurrences of indexed code symbol names and
//! emits `REFERENCES_CODE_*` edges. These edges are the architectural
//! keystone of the brain — once present, a single PPR run over the
//! unified scope (which now includes them; see `GraphScope::unified`)
//! ranks "Auth Service Design.md" and `AuthService::authenticate`
//! together for a query seeded with either.
//!
//! Strategy: name-match with word boundaries. Two passes per note:
//!
//! 1. Whole-note pass — emit `REFERENCES_CODE_NOTE_TO_SYMBOL` for every
//!    symbol whose name appears anywhere in the body. Coarse but cheap.
//! 2. Per-section pass — for each Section, look at its specific text
//!    slice and emit `REFERENCES_CODE_SECTION_TO_SYMBOL` for symbols
//!    that appear there. Finer-grained.
//!
//! Symbols whose name length is < 4 are skipped — they collide too
//! easily with English words ("Get", "Set", "id", "ok"). This is the
//! single most important false-positive filter. The architecture doc
//! §10.1 has the long-form rationale.
//!
//! Confidence scoring:
//! - Function = 0.9 (most distinctive)
//! - Class    = 0.8
//! - Interface = 0.8
//! - Method   = 0.7 (often generic verbs after the dedup filter)
//!
//! Performance: O(notes × avg_body_len + symbols) per discovery pass.
//! We build a `HashSet<&str>` over symbol names once, then walk each
//! note body once tokenising on word boundaries and probing the set.
//! No regex, no Aho-Corasick — string interning + hash lookup beats
//! both at this graph size.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
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

/// Like `discover_cross_domain_links` but honours the provided `CrossDomainConfig`.
pub fn discover_cross_domain_links_with_config(
    store: &GraphStore,
    config: &CrossDomainConfig,
) -> Result<CrossDomainResult, anyhow::Error> {
    discover_cross_domain_links_full(store, config, &VaultReaders::new())
}

/// Full implementation accepting both config and vault readers.
fn discover_cross_domain_links_full(
    store: &GraphStore,
    config: &CrossDomainConfig,
    vault_readers: &VaultReaders<'_>,
) -> Result<CrossDomainResult, anyhow::Error> {
    let symbols = store
        .list_all_symbols_lite()
        .context("list_all_symbols_lite")?;
    if symbols.is_empty() {
        // No code indexed — nothing to bridge to. Not an error.
        return Ok(CrossDomainResult::default());
    }

    let index = SymbolIndex::build_with_config(&symbols, config);
    if index.is_empty() {
        return Ok(CrossDomainResult::default());
    }

    let notes = store.list_notes(None).context("list_notes")?;
    let mut result = CrossDomainResult::default();

    // Scan all notes in memory first (no DB writes), then flush in
    // transaction-batched chunks. Earlier versions committed once per
    // note × section, which on macOS amounts to thousands of fsync'd
    // commits and dominates indexing time. NOTES_PER_TXN bounds peak
    // transaction memory while still amortising fsync cost.
    const NOTES_PER_TXN: usize = 100;

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
/// transaction so the heavy regex/index work runs lock-free, then
/// flushed in batched transactions by `flush_scanned_notes`.
struct ScannedNote {
    note_uid: String,
    note_edges: Vec<(String, String, f32, &'static str)>,
    section_edges: Vec<(String, String, f32, &'static str)>,
}

enum ScanOutcome {
    Scanned(ScannedNote),
    Skipped,
}

/// Flush a batch of scanned notes inside a single write transaction:
/// delete each note's existing cross-domain edges, then bulk-insert the
/// fresh ones. One fsync per batch, not per note × section.
///
/// Store errors are folded INTO the message (`{op}: {cause}`) rather than
/// attached via `anyhow::Context`: callers log discovery failures with `{e}`
/// (Display of the outermost error only), so a plain context would reduce the
/// warning to a bare function name and hide the underlying cause — e.g.
/// `Cannot execute write operations in a read-only database!`, which is what
/// `brain add` hits because its discovery store is opened read-only.
fn flush_scanned_notes(
    store: &GraphStore,
    batch: &[ScannedNote],
    result: &mut CrossDomainResult,
) -> Result<(), anyhow::Error> {
    let conn = store
        .begin_transaction()
        .map_err(|e| anyhow::anyhow!("begin_transaction for cross-domain flush: {e}"))?;

    for scanned in batch {
        nestweaver_store::GraphStore::delete_cross_domain_edges_for_note_on(
            &conn,
            &scanned.note_uid,
        )
        .map_err(|e| anyhow::anyhow!("delete_cross_domain_edges_for_note_on: {e}"))?;

        if !scanned.note_edges.is_empty() {
            let refs: Vec<(&str, &str, f32, &str)> = scanned
                .note_edges
                .iter()
                .map(|(n, s, c, src)| (n.as_str(), s.as_str(), *c, *src))
                .collect();
            nestweaver_store::GraphStore::batch_insert_note_to_symbol_edges_on(&conn, &refs)
                .map_err(|e| anyhow::anyhow!("batch_insert_note_to_symbol_edges_on: {e}"))?;
        }

        if !scanned.section_edges.is_empty() {
            let refs: Vec<(&str, &str, f32, &str)> = scanned
                .section_edges
                .iter()
                .map(|(s, sym, c, src)| (s.as_str(), sym.as_str(), *c, *src))
                .collect();
            nestweaver_store::GraphStore::batch_insert_section_to_symbol_edges_on(&conn, &refs)
                .map_err(|e| anyhow::anyhow!("batch_insert_section_to_symbol_edges_on: {e}"))?;
        }

        result.notes_scanned += 1;
        result.note_to_symbol_edges += scanned.note_edges.len();
        result.section_to_symbol_edges += scanned.section_edges.len();
    }

    store
        .commit_transaction(&conn)
        .map_err(|e| anyhow::anyhow!("commit_transaction for cross-domain flush: {e}"))?;
    Ok(())
}

/// Build a SymbolIndex from the store's symbol list. Pre-build once per
/// batch to avoid redundant DB queries when processing multiple notes.
pub fn build_symbol_index(store: &GraphStore) -> Result<SymbolIndex, anyhow::Error> {
    build_symbol_index_with_config(store, &CrossDomainConfig::default())
}

/// Like `build_symbol_index` but honours the provided `CrossDomainConfig`.
pub fn build_symbol_index_with_config(
    store: &GraphStore,
    config: &CrossDomainConfig,
) -> Result<SymbolIndex, anyhow::Error> {
    let symbols = store
        .list_all_symbols_lite()
        .context("list_all_symbols_lite")?;
    Ok(SymbolIndex::build_with_config(&symbols, config))
}

/// Rebuild cross-domain links for one note, using a pre-built SymbolIndex.
/// Avoids the per-note DB query for symbols that `discover_cross_domain_links_for_note`
/// performs. Returns the count of (note_edges, section_edges) emitted.
pub fn discover_cross_domain_links_for_note_with_index(
    store: &GraphStore,
    note_uid: &str,
    index: &SymbolIndex,
) -> Result<(usize, usize), anyhow::Error> {
    discover_cross_domain_links_for_note_with_index_and_readers(
        store,
        note_uid,
        index,
        &VaultReaders::new(),
    )
}

/// Like [`discover_cross_domain_links_for_note_with_index`] but accepts
/// [`VaultReaders`] for server-mode bare-clone support.
pub fn discover_cross_domain_links_for_note_with_index_and_readers(
    store: &GraphStore,
    note_uid: &str,
    index: &SymbolIndex,
    vault_readers: &VaultReaders<'_>,
) -> Result<(usize, usize), anyhow::Error> {
    if index.is_empty() {
        return Ok((0, 0));
    }
    let note = store.lookup_note(note_uid).context("lookup_note")?;
    let outcome = discover_one_note(store, &note, index, vault_readers)?;
    match outcome {
        NoteOutcome::Indexed {
            note_edges,
            section_edges,
        } => Ok((note_edges, section_edges)),
        NoteOutcome::Skipped => Ok((0, 0)),
    }
}

/// Rebuild cross-domain links for one note only. Called by the watcher
/// after re-indexing a saved file so the bridges reflect the current
/// body content. Returns the count of edges emitted.
pub fn discover_cross_domain_links_for_note(
    store: &GraphStore,
    note_uid: &str,
) -> Result<(usize, usize), anyhow::Error> {
    let symbols = store
        .list_all_symbols_lite()
        .context("list_all_symbols_lite")?;
    if symbols.is_empty() {
        return Ok((0, 0));
    }
    let index = SymbolIndex::build_with_config(&symbols, &CrossDomainConfig::default());
    if index.is_empty() {
        return Ok((0, 0));
    }
    let note = store.lookup_note(note_uid).context("lookup_note")?;
    let outcome = discover_one_note(store, &note, &index, &VaultReaders::new())?;
    match outcome {
        NoteOutcome::Indexed {
            note_edges,
            section_edges,
        } => Ok((note_edges, section_edges)),
        NoteOutcome::Skipped => Ok((0, 0)),
    }
}

enum NoteOutcome {
    Indexed {
        note_edges: usize,
        section_edges: usize,
    },
    Skipped,
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

    // Whole-note pass.
    let note_matches = index.scan(&body);
    let mut note_edges: Vec<(String, String, f32, &'static str)> =
        Vec::with_capacity(note_matches.len());
    for (sym_uid, conf) in &note_matches {
        note_edges.push((note.uid.clone(), sym_uid.clone(), *conf, "name-match"));
    }

    // Per-section pass.
    let sections = store.sections_in_note(&note.uid).unwrap_or_default();
    let body_lines: Vec<&str> = body.lines().collect();
    let mut section_edges: Vec<(String, String, f32, &'static str)> = Vec::new();
    for sec in &sections {
        let text = slice_body_lines(&body_lines, sec.start_line, sec.end_line);
        if text.trim().is_empty() {
            continue;
        }
        for (sym_uid, conf) in index.scan(&text) {
            section_edges.push((sec.uid.clone(), sym_uid, conf, "name-match"));
        }
    }

    Ok(ScanOutcome::Scanned(ScannedNote {
        note_uid: note.uid.clone(),
        note_edges,
        section_edges,
    }))
}

fn discover_one_note(
    store: &GraphStore,
    note: &nestweaver_schema::Note,
    index: &SymbolIndex,
    vault_readers: &VaultReaders<'_>,
) -> Result<NoteOutcome, anyhow::Error> {
    // Load the note body via ContentReader (server/bare-clone mode) or
    // filesystem fallback (local/daemon mode).
    let body = match read_note_body(store, note, vault_readers) {
        Some(s) => s,
        None => return Ok(NoteOutcome::Skipped),
    };

    // Delete existing cross-domain edges before re-emitting (idempotency).
    store
        .delete_cross_domain_edges_for_note(&note.uid)
        .context("delete_cross_domain_edges_for_note")?;

    // ── Whole-note pass — coarse edges to every symbol mentioned ────────
    let note_matches = index.scan(&body);
    let mut note_edges: Vec<(String, String, f32, &str)> = Vec::new();
    for (sym_uid, conf) in &note_matches {
        note_edges.push((note.uid.clone(), sym_uid.clone(), *conf, "name-match"));
    }
    let n_note_edges = note_edges.len();
    if !note_edges.is_empty() {
        let refs: Vec<(&str, &str, f32, &str)> = note_edges
            .iter()
            .map(|(n, s, c, src)| (n.as_str(), s.as_str(), *c, *src))
            .collect();
        store
            .batch_insert_note_to_symbol_edges(&refs)
            .context("batch_insert_note_to_symbol_edges")?;
    }

    // ── Per-section pass — finer-grained edges scoped to section text ───
    let sections = store.sections_in_note(&note.uid).unwrap_or_default();
    let body_lines: Vec<&str> = body.lines().collect();
    let mut sec_edges: Vec<(String, String, f32, &str)> = Vec::new();
    for sec in &sections {
        let text = slice_body_lines(&body_lines, sec.start_line, sec.end_line);
        if text.trim().is_empty() {
            continue;
        }
        for (sym_uid, conf) in index.scan(&text) {
            sec_edges.push((sec.uid.clone(), sym_uid, conf, "name-match"));
        }
    }
    let n_sec_edges = sec_edges.len();
    if !sec_edges.is_empty() {
        let refs: Vec<(&str, &str, f32, &str)> = sec_edges
            .iter()
            .map(|(s, sym, c, src)| (s.as_str(), sym.as_str(), *c, *src))
            .collect();
        store
            .batch_insert_section_to_symbol_edges(&refs)
            .context("batch_insert_section_to_symbol_edges")?;
    }

    Ok(NoteOutcome::Indexed {
        note_edges: n_note_edges,
        section_edges: n_sec_edges,
    })
}

/// Concatenate body lines `[start..=end]` (1-based, inclusive). Falls
/// back to empty string for out-of-range inputs.
fn slice_body_lines(lines: &[&str], start: u32, end: u32) -> String {
    if start == 0 || start as usize > lines.len() {
        return String::new();
    }
    let end = (end as usize).min(lines.len());
    let start = (start - 1) as usize;
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

/// Symbol-name → list of (uid, confidence) candidates. Built once per
/// discovery pass. The HashMap lookup is the inner hot loop.
pub struct SymbolIndex {
    by_name: HashMap<String, Vec<(String, f32)>>,
}

impl SymbolIndex {
    fn build_with_config(symbols: &[(String, String, String)], config: &CrossDomainConfig) -> Self {
        // Compute effective stoplist: replace entirely or extend the built-in.
        let effective_stoplist: HashSet<&str> = if let Some(replace) = &config.stoplist_replace {
            replace.iter().map(|s| s.as_str()).collect()
        } else {
            let mut set: HashSet<&str> = STOPLIST.iter().copied().collect();
            for word in &config.stoplist_extend {
                set.insert(word.as_str());
            }
            set
        };

        let min_len = config.min_symbol_name_length.unwrap_or(MIN_SYMBOL_NAME_LEN);

        let mut by_name: HashMap<String, Vec<(String, f32)>> = HashMap::new();
        for (uid, name, kind) in symbols {
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
            let conf = match kind.as_str() {
                "Function" => 0.9_f32,
                "Class" => 0.8,
                "Interface" => 0.8,
                "Method" => 0.7,
                _ => 0.6,
            };
            by_name
                .entry(name.clone())
                .or_default()
                .push((uid.clone(), conf));
        }
        Self { by_name }
    }

    fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Walk `text` token-by-token on word boundaries; for each token,
    /// look up the symbol name set. Returns (sym_uid, confidence) for
    /// each distinct (uid) hit. A symbol mentioned twice in the same
    /// text appears once in the output — duplicate edges would inflate
    /// PPR scores without semantic justification.
    fn scan(&self, text: &str) -> Vec<(String, f32)> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out: Vec<(String, f32)> = Vec::new();
        for token in tokenize(text) {
            if let Some(candidates) = self.by_name.get(token) {
                for (uid, conf) in candidates {
                    if seen.insert(uid.as_str()) {
                        out.push((uid.clone(), *conf));
                    }
                }
            }
        }
        out
    }
}

/// Split `text` into identifier-shaped tokens. Word boundary = any
/// non-alphanumeric, non-underscore character.
///
/// We DON'T split on `.` or `::` so that `Foo::bar` or `obj.method` are
/// tokenised as `Foo`, `bar` / `obj`, `method` — matching how a code
/// indexer stores symbol names.
fn tokenize(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{
        Note, NoteKind, Symbol, SymbolKind, Vault, Visibility, repo_uid, symbol_uid, vault_uid,
    };
    use tempfile::tempdir;

    #[test]
    fn tokenize_splits_on_word_boundaries() {
        let tokens: Vec<&str> = tokenize("foo bar.baz_qux Class::method").collect();
        assert_eq!(tokens, vec!["foo", "bar", "baz_qux", "Class", "method"]);
    }

    #[test]
    fn symbol_index_skips_short_names() {
        let symbols = vec![
            (
                "sym:1".to_string(),
                "Get".to_string(),
                "Function".to_string(),
            ),
            (
                "sym:2".to_string(),
                "Processor".to_string(),
                "Class".to_string(),
            ),
        ];
        let idx = SymbolIndex::build_with_config(&symbols, &CrossDomainConfig::default());
        assert!(
            idx.scan("calls Get and Processor")
                .iter()
                .any(|(u, _)| u == "sym:2")
        );
        // "Get" is below MIN length and must NOT appear.
        assert!(!idx.scan("Get").iter().any(|(u, _)| u == "sym:1"));
    }

    #[test]
    fn symbol_index_dedupes_within_a_text() {
        let symbols = vec![(
            "sym:x".to_string(),
            "Authenticator".to_string(),
            "Class".to_string(),
        )];
        let idx = SymbolIndex::build_with_config(&symbols, &CrossDomainConfig::default());
        let hits = idx.scan("Authenticator and Authenticator and Authenticator");
        assert_eq!(hits.len(), 1);
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
            msg.contains("delete_cross_domain_edges_for_note_on"),
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
        let symbols = vec![
            (
                "sym:1".to_string(),
                "Error".to_string(),
                "Class".to_string(),
            ),
            (
                "sym:2".to_string(),
                "AuthService".to_string(),
                "Class".to_string(),
            ),
        ];
        let idx = SymbolIndex::build_with_config(&symbols, &CrossDomainConfig::default());
        let hits = idx.scan("Error and AuthService");
        assert_eq!(hits.len(), 1, "only AuthService should match");
        assert_eq!(hits[0].0, "sym:2");
    }

    #[test]
    fn stoplist_is_case_insensitive() {
        for name in ["error", "ERROR", "Error", "eRrOr"] {
            let symbols = vec![("sym:1".to_string(), name.to_string(), "Class".to_string())];
            let idx = SymbolIndex::build_with_config(&symbols, &CrossDomainConfig::default());
            assert!(idx.is_empty(), "'{name}' should be stopped");
        }
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
}
