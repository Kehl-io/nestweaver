//! Symbol-window reads (RFC F5).
//!
//! Returns just a symbol's source span (`start_line..=end_line`, from P0.1's
//! `Symbol.end_line`) instead of a whole file — the cheapest token cut in the
//! agent loop. Optionally includes adjacent symbols in the same file
//! (`neighbors`) and is token-budget aware. Comment stripping is a planned
//! follow-up (default-off; deferred to avoid false-elision risk).

use std::path::Path;

use nestweaver_schema::Symbol;
use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};

use crate::content_reader::ContentReader;

/// One returned symbol window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolWindow {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    /// False when the source span could not be read (file not found from the
    /// reader's working directory, or an out-of-range span) — the body is then
    /// an empty string that would otherwise be indistinguishable from a genuinely
    /// empty symbol. Callers should pass `root` or run from the repo to fix it.
    pub body_available: bool,
    /// True when this symbol was pulled in via `neighbors`, not requested directly.
    pub is_neighbor: bool,
}

/// A spec that matched more than one symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousMatch {
    pub query: String,
    pub candidate_uids: Vec<String>,
}

// `Deserialize` so the CLI can parse a daemon response back into the SAME type
// the local path produces, and therefore share one rendering and exit-code
// path instead of short-circuiting on the daemon branch (nw-186).
// `serde(default)` keeps an older daemon that omits a newer field readable.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadSymbolsResult {
    pub symbols: Vec<SymbolWindow>,
    /// Specs that resolved to no symbol.
    pub not_found: Vec<String>,
    /// Specs that resolved to multiple symbols (caller should disambiguate by UID).
    pub ambiguous: Vec<AmbiguousMatch>,
    /// UIDs dropped because the token budget was exhausted.
    pub dropped: Vec<String>,
    pub truncated: bool,
    /// Set when the FIRST symbol alone exceeded `token_budget`.
    ///
    /// One symbol is always returned, because an empty answer to "read this
    /// symbol" is useless. That guarantee is deliberate — but it was kept
    /// silently, with `truncated: false`, so a caller asking for 1 token
    /// received ~6,700 and had no way to know the budget had been ignored
    /// (nw-111). The guarantee stays; the silence does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_exceeded_by_first_symbol: Option<BudgetOverrun>,
}

/// How far the mandatory first symbol overran the requested budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetOverrun {
    /// The budget the caller asked for.
    pub requested_tokens: usize,
    /// Estimated tokens actually returned for that first symbol.
    pub returned_tokens: usize,
    pub note: String,
}

/// Read lines `start..=end` (1-based, inclusive) from `file_path` via the reader.
fn read_span(reader: &dyn ContentReader, file_path: &str, start: u32, end: u32) -> Option<String> {
    let text = reader.read_file(Path::new(file_path)).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    if start == 0 || start as usize > lines.len() {
        return None;
    }
    let lo = (start - 1) as usize;
    // `end` is 1-based inclusive; clamp to the file and never below `start`
    // (old DBs may carry end_line = 0 before a `index --force`).
    let hi = (end.max(start) as usize).min(lines.len());
    Some(lines[lo..hi].join("\n"))
}

/// Resolve a spec (`sym:` UID, bare name, or dotted/`::` FQN) to candidate symbols.
fn resolve(store: &GraphStore, spec: &str) -> Vec<Symbol> {
    if spec.starts_with("sym:") {
        return store.lookup_symbol(spec).ok().into_iter().collect();
    }
    // FQN forms: take the last path segment as the symbol name.
    let name = spec
        .rsplit("::")
        .next()
        .unwrap_or(spec)
        .rsplit('.')
        .next()
        .unwrap_or(spec);
    store.lookup_symbols_by_name(name).unwrap_or_default()
}

/// Estimate the token cost of a window (chars/4 + small metadata overhead).
fn window_cost(body: &str) -> usize {
    body.len() / 4 + 16
}

pub fn read_symbols(
    store: &GraphStore,
    specs: &[String],
    reader: &dyn ContentReader,
    neighbors: u8,
    token_budget: Option<usize>,
) -> ReadSymbolsResult {
    let mut result = ReadSymbolsResult::default();

    // 1. Resolve specs → primary symbols (preserving input order).
    let mut primary: Vec<Symbol> = Vec::new();
    for spec in specs {
        let candidates = resolve(store, spec);
        match candidates.len() {
            0 => result.not_found.push(spec.clone()),
            1 => primary.push(candidates.into_iter().next().expect("len == 1")),
            _ => result.ambiguous.push(AmbiguousMatch {
                query: spec.clone(),
                candidate_uids: candidates.iter().map(|s| s.uid.clone()).collect(),
            }),
        }
    }

    // 2. Expand with neighbours (adjacent symbols in the same file), de-duped.
    let mut ordered: Vec<(Symbol, bool)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for sym in primary {
        if neighbors > 0 {
            let mut in_file = store.symbols_in_file(&sym.file_path).unwrap_or_default();
            in_file.sort_by_key(|s| s.start_line);
            if let Some(idx) = in_file.iter().position(|s| s.uid == sym.uid) {
                let lo = idx.saturating_sub(neighbors as usize);
                let hi = (idx + neighbors as usize).min(in_file.len().saturating_sub(1));
                for (j, n) in in_file.into_iter().enumerate() {
                    if j < lo || j > hi {
                        continue;
                    }
                    let is_self = n.uid == sym.uid;
                    if seen.insert(n.uid.clone()) {
                        ordered.push((n, !is_self));
                    }
                }
                continue;
            }
        }
        if seen.insert(sym.uid.clone()) {
            ordered.push((sym, false));
        }
    }

    // 3. Build windows, honoring the token budget (input order).
    let mut used = 0usize;
    for (sym, is_neighbor) in ordered {
        let body_opt = read_span(reader, &sym.file_path, sym.start_line, sym.end_line);
        let body_available = body_opt.is_some();
        let body = body_opt.unwrap_or_default();
        let cost = window_cost(&body);
        if let Some(budget) = token_budget
            && !result.symbols.is_empty()
            && used + cost > budget
        {
            result.dropped.push(sym.uid.clone());
            result.truncated = true;
            continue;
        }
        // The first symbol is exempt from the budget so the caller never gets an
        // empty answer — but say so rather than reporting a clean result that
        // silently blew the budget.
        if let Some(budget) = token_budget
            && result.symbols.is_empty()
            && cost > budget
        {
            result.truncated = true;
            result.budget_exceeded_by_first_symbol = Some(BudgetOverrun {
                requested_tokens: budget,
                returned_tokens: cost,
                note: format!(
                    "the first symbol alone costs ~{cost} tokens, over the requested \
                     budget of {budget}; it is returned whole because an empty result \
                     answers nothing, so this response EXCEEDS the budget"
                ),
            });
        }
        used += cost;
        result.symbols.push(SymbolWindow {
            uid: sym.uid,
            name: sym.name,
            kind: sym.kind.to_string(),
            path: sym.file_path,
            start_line: sym.start_line,
            end_line: sym.end_line,
            body,
            body_available,
            is_neighbor,
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_reader::FilesystemReader;
    use crate::index::index_directory_in_memory;
    use std::fs;

    fn test_repo() -> (tempfile::TempDir, std::path::PathBuf, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(name) {\n  return hello(name);\n}\nfunction hello(n) {\n  return n;\n}\n",
        )
        .unwrap();
        let (_r, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        (dir, src, store)
    }

    /// nw-111 (3): a budget too small for even one symbol must be DISCLOSED.
    ///
    /// The first symbol is deliberately exempt so the caller never receives an
    /// empty answer to "read this symbol". But that exemption was silent: a
    /// 1-token request returned the whole body with `truncated: false`, and the
    /// reported output was byte-identical at budgets of 1, 50, 500, 5000 and
    /// 16000. The guarantee is kept; the silence is not.
    #[test]
    fn a_budget_smaller_than_the_first_symbol_is_disclosed() {
        let (_dir, src, store) = test_repo();
        let reader = FilesystemReader::new(&src);

        let res = read_symbols(&store, &["greet".to_string()], &reader, 0, Some(1));

        assert_eq!(
            res.symbols.len(),
            1,
            "the first symbol is still returned — an empty result answers nothing"
        );
        assert!(
            res.truncated,
            "a response that exceeds the requested budget is not a clean result"
        );
        let overrun = res
            .budget_exceeded_by_first_symbol
            .as_ref()
            .expect("the overrun must be reported");
        assert_eq!(overrun.requested_tokens, 1);
        assert!(
            overrun.returned_tokens > 1,
            "must state what was actually returned: {overrun:?}"
        );
        assert!(
            overrun.note.contains("EXCEEDS"),
            "the note must say plainly that the budget was exceeded: {}",
            overrun.note
        );
    }

    /// A budget that comfortably fits must stay clean — a disclosure that always
    /// fires is one callers learn to ignore.
    #[test]
    fn a_sufficient_budget_reports_no_overrun() {
        let (_dir, src, store) = test_repo();
        let reader = FilesystemReader::new(&src);

        let res = read_symbols(&store, &["greet".to_string()], &reader, 0, Some(10_000));

        assert_eq!(res.symbols.len(), 1);
        assert!(!res.truncated, "nothing was dropped or overrun");
        assert!(
            res.budget_exceeded_by_first_symbol.is_none(),
            "must not cry wolf: {:?}",
            res.budget_exceeded_by_first_symbol
        );
    }

    #[test]
    fn read_symbols_returns_the_symbol_span_body() {
        let (_dir, src, store) = test_repo();
        let reader = FilesystemReader::new(&src);
        let res = read_symbols(&store, &["greet".to_string()], &reader, 0, None);
        assert_eq!(res.symbols.len(), 1, "should resolve 'greet'");
        let w = &res.symbols[0];
        assert!(
            w.body.contains("function greet") && w.body.contains("return hello"),
            "body should be the greet span, got: {:?}",
            w.body
        );
        assert!(
            !w.body.contains("function hello"),
            "body should NOT spill into the next function: {:?}",
            w.body
        );
        assert!(w.end_line > w.start_line, "multi-line span");
        assert!(w.body_available, "body was read, so body_available is true");
    }

    #[test]
    fn read_symbols_flags_unreadable_body() {
        // nw-084: reading with a reader rooted at a directory that doesn't
        // contain the source file yields an empty body — flag it as unavailable
        // so callers can tell it apart from a genuinely empty symbol.
        let (_dir, _src, store) = test_repo();
        let wrong_root = tempfile::tempdir().unwrap(); // no source files here
        let reader = FilesystemReader::new(wrong_root.path());
        let res = read_symbols(&store, &["greet".to_string()], &reader, 0, None);
        assert_eq!(res.symbols.len(), 1, "symbol still resolves from the graph");
        let w = &res.symbols[0];
        assert!(
            w.body.is_empty(),
            "body is empty when the file can't be read"
        );
        assert!(
            !w.body_available,
            "an unreadable source span must set body_available = false"
        );
    }
}
