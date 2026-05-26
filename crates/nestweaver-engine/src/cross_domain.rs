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
pub fn discover_cross_domain_links(store: &GraphStore) -> Result<CrossDomainResult, anyhow::Error> {
    discover_cross_domain_links_with_config(store, &CrossDomainConfig::default())
}

/// Like `discover_cross_domain_links` but honours the provided `CrossDomainConfig`.
pub fn discover_cross_domain_links_with_config(
    store: &GraphStore,
    config: &CrossDomainConfig,
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

    for note in &notes {
        let outcome = discover_one_note(store, note, &index)?;
        match outcome {
            NoteOutcome::Indexed {
                note_edges,
                section_edges,
            } => {
                result.notes_scanned += 1;
                result.note_to_symbol_edges += note_edges;
                result.section_to_symbol_edges += section_edges;
            }
            NoteOutcome::Skipped => {
                result.skipped_unreadable += 1;
            }
        }
    }

    Ok(result)
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
    if index.is_empty() {
        return Ok((0, 0));
    }
    let note = store.lookup_note(note_uid).context("lookup_note")?;
    let outcome = discover_one_note(store, &note, index)?;
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
    let outcome = discover_one_note(store, &note, &index)?;
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

fn discover_one_note(
    store: &GraphStore,
    note: &nestweaver_schema::Note,
    index: &SymbolIndex,
) -> Result<NoteOutcome, anyhow::Error> {
    // Load the note body from disk via its vault root.
    let Ok(vault) = store.lookup_vault(&note.vault_uid) else {
        return Ok(NoteOutcome::Skipped);
    };
    let path = Path::new(&vault.root_path).join(&note.file_path);
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(NoteOutcome::Skipped),
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
                created_at: None,
                modified_at: None,
                pagerank_score: None,
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
                created_at: None,
                modified_at: None,
                pagerank_score: None,
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
