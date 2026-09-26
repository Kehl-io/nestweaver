//! Symbol-window reads (RFC F5).
//!
//! Returns just a symbol's source span (`start_line..=end_line`, from P0.1's
//! `Symbol.end_line`) instead of a whole file — the cheapest token cut in the
//! agent loop. Optionally includes adjacent symbols in the same file
//! (`neighbors`) and is token-budget aware. Comment stripping is a planned
//! follow-up (default-off; deferred to avoid false-elision risk).

use std::collections::HashMap;
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
    /// nw-689: the stored span no longer holds this symbol (the repo changed
    /// since indexing) and it could not be re-located; `body` is empty and
    /// `body_available` is false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale_span: bool,
    /// nw-689: the current parsed span differs from the indexed start or end.
    /// This is the originally stored start (which may equal `start_line` when
    /// only the end changed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relocated_from_line: Option<u32>,
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

/// Read lines `start..=end` (1-based, inclusive) from one already-read file.
fn read_span(text: &str, start: u32, end: u32) -> Option<String> {
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

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Only identifier-shaped names are drift-checked. Test-block names
/// (`describe('renders the list')`), FQNs and operator names pass unchanged.
fn is_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_ident_char)
}

/// Byte offsets of every whole-word occurrence of `name` in `line`.
fn whole_word_hits<'a>(line: &'a str, name: &'a str) -> impl Iterator<Item = usize> + 'a {
    line.match_indices(name).filter_map(move |(i, _)| {
        let before = line[..i].chars().next_back();
        let after = line[i + name.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !is_ident_char(c));
        (boundary(before) && boundary(after)).then_some(i)
    })
}

fn names_symbol(line: &str, name: &str) -> bool {
    whole_word_hits(line, name).next().is_some()
}

/// How many lines this checks after the start of the declaration.
const HEADER_LINES: usize = 5;

/// nw-689: does `body` (the stored span, read from disk now) still hold the
/// symbol `name`? A `false` means the file changed since indexing and the span
/// now holds some other code.
///
/// The rule is "the name appears as a whole word in the declaration header,
/// within [`HEADER_LINES`] lines after any annotation block", widened for
/// spans whose declaration puts the name elsewhere:
///
/// - a leading annotation/decorator/attribute block (Java and Dart fold
///   annotations INTO the declaration node, and a multi-line `@GetMapping(...)`
///   or `@ApiResponses({...})` pushes the name well past line 5) is skipped,
///   and the window starts at the declaration after it;
/// - the LAST line of the span may carry the name (C `typedef struct { ... }
///   Name;`);
/// - a file-level component symbol (Svelte/Vue/Astro) is named after the file
///   stem and may never name itself in its source.
fn span_holds_symbol(body: &str, name: &str, file_path: &str, signature: &str) -> bool {
    if !is_identifier(name) {
        return true;
    }
    let file_component = file_stem(file_path) == name
        && ["vue", "svelte", "astro"].contains(&file_path.rsplit('.').next().unwrap_or(""));
    // Script-setup and file-level component signatures are synthetic; they
    // have no literal declaration line to compare against the source.
    if file_component && signature.starts_with('<') {
        return true;
    }
    let lines: Vec<&str> = body.lines().collect();
    // The parser has already identified the current declaration and its
    // exact extent. The indexed signature may include an old one-line body,
    // so comparing it to the current first line would reject a valid edit.
    // An Options API component is named after its .vue file. Its real
    // declaration says `export default`, never the component name.
    if file_component {
        return true;
    }
    let decl = declaration_start(&lines);
    // Inspect the declaration header only. A call or comment inside the body
    // can mention the old name while this span now belongs to another method.
    for line in lines.iter().skip(decl).take(HEADER_LINES) {
        if names_symbol(line, name) {
            return true;
        }
        if line.contains('{') || line.contains(';') || line.contains("=>") {
            break;
        }
    }
    signature.starts_with("typedef") && lines.last().is_some_and(|l| names_symbol(l, name))
}

fn file_stem(file_path: &str) -> &str {
    let file_name = file_path.rsplit(['/', '\\']).next().unwrap_or(file_path);
    match file_name.rfind('.') {
        Some(0) | None => file_name,
        Some(dot) => &file_name[..dot],
    }
}

/// Index of the first line after a leading block of annotations/attributes
/// (`@Foo(...)`, `#[attr]`, C# `[Attr]`), blank lines and comments. Bracket
/// depth is tracked so a multi-line annotation argument list is skipped whole.
/// Skipping only moves where the name is looked for; it never accepts a span
/// on its own.
fn declaration_start(lines: &[&str]) -> usize {
    let mut depth: i32 = 0;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let is_comment = t.starts_with("//") || t.starts_with("/*") || t.starts_with('*');
        let is_prefix = depth > 0
            || t.is_empty()
            || is_comment
            || t.starts_with('@')
            || t.starts_with("#[")
            || t.starts_with('[');
        if !is_prefix {
            return i;
        }
        if !is_comment {
            for c in t.chars() {
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
        }
    }
    lines.len()
}

/// Keywords that, immediately before a name, make a line a definition of it.
const DEFINITION_KEYWORDS: &[&str] = &[
    "fn",
    "function",
    "def",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
    "const",
    "let",
    "var",
    "impl",
    "func",
    "fun",
    "val",
    "static",
    "mod",
    "module",
    "namespace",
    "object",
    "record",
    "union",
    "protocol",
];

/// Is `line` a definition of `name`? The word immediately before the name
/// must be a definition keyword (`pub async fn NAME`, `export const NAME`),
/// or the line is a Go receiver method (`func (s *S) NAME(`). A keyword
/// merely appearing earlier on the line is not enough: `let x = NAME();` is a
/// call, and relocating to it would return exactly the wrong body nw-689 is
/// about.
fn defines_symbol(line: &str, name: &str) -> bool {
    let go_receiver = {
        let t = line.trim_start();
        t.starts_with("func (") || t.starts_with("func(")
    };
    whole_word_hits(line, name).any(|i| {
        let prefix = line[..i].trim_end();
        if go_receiver && prefix.ends_with(')') {
            return true;
        }
        let prev = prefix
            .rsplit(|c: char| !is_ident_char(c))
            .next()
            .unwrap_or("");
        !prev.is_empty() && prefix.ends_with(prev) && DEFINITION_KEYWORDS.contains(&prev)
    })
}

/// (start, end), 1-based, of the UNIQUE definition line for `name` in `text`,
/// keeping a window of `len` lines. `None` when there is no such line or more
/// than one: a guess between two definitions is how the wrong body gets
/// returned.
fn relocate(text: &str, name: &str, len: u32) -> Option<(u32, u32)> {
    let mut hits = text
        .lines()
        .enumerate()
        .filter(|(_, l)| defines_symbol(l, name))
        .map(|(i, _)| i);
    let only = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    let start = u32::try_from(only).ok()?.checked_add(1)?;
    Some((start, start + len.max(1) - 1))
}

/// Re-parse the current file to verify the symbol's actual start and end.
/// The indexed line count is not safe after a file edit: a shorter definition
/// can pull the next symbol into the window, and a longer one is truncated.
fn verified_current_span(
    text: &str,
    parsed: &nestweaver_parser::ParsedFile,
    sym: &Symbol,
) -> Option<(u32, u32, String)> {
    let candidates: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|raw| raw.name == sym.name && raw.kind == sym.kind)
        .collect();
    let hash_matches: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|raw| raw.content_hash == sym.content_hash)
        .collect();
    let raw = match hash_matches.as_slice() {
        [only] => *only,
        many if many.len() > 1 => *many.iter().find(|raw| raw.start_line == sym.start_line)?,
        _ => match candidates.as_slice() {
            [only] => *only,
            _ => return None,
        },
    };
    if raw.start_line != sym.start_line {
        let old_len = sym.end_line.max(sym.start_line) - sym.start_line + 1;
        if relocate(text, &sym.name, old_len).map(|(start, _)| start) != Some(raw.start_line) {
            return None;
        }
    }
    let body = read_span(text, raw.start_line, raw.end_line)?;
    span_holds_symbol(&body, &sym.name, &sym.file_path, &sym.signature).then_some((
        raw.start_line,
        raw.end_line,
        body,
    ))
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
    read_symbols_budgeted(store, specs, reader, neighbors, token_budget, true)
}

/// Like [`read_symbols`], with explicit control over the first-symbol
/// exemption.
///
/// `exempt_first_symbol` is the nw-111 guarantee: a single-call read never
/// returns an empty `symbols` list just because the first window overruns
/// `token_budget`. Callers that merge per-repo groups must pass `false` once
/// any window has already been returned, otherwise a later repo re-applies
/// the exemption after the budget is already spent (nw-542).
pub fn read_symbols_budgeted(
    store: &GraphStore,
    specs: &[String],
    reader: &dyn ContentReader,
    neighbors: u8,
    token_budget: Option<usize>,
    exempt_first_symbol: bool,
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
    // Neighbor windows commonly share a file. Parse it once per call.
    let mut file_cache: HashMap<String, Option<(String, Option<nestweaver_parser::ParsedFile>)>> =
        HashMap::new();
    for (sym, is_neighbor) in ordered {
        let mut start_line = sym.start_line;
        let mut end_line = sym.end_line;
        let mut stale_span = false;
        let mut relocated_from_line = None;
        let current = file_cache.entry(sym.file_path.clone()).or_insert_with(|| {
            reader
                .read_file(Path::new(&sym.file_path))
                .ok()
                .map(|text| {
                    let parsed =
                        nestweaver_parser::parse_source(Path::new(&sym.file_path), &text).ok();
                    (text, parsed)
                })
        });
        let body_opt = current.as_ref().and_then(|(text, parsed)| {
            match parsed
                .as_ref()
                .and_then(|parsed| verified_current_span(text, parsed, &sym))
            {
                Some((current_start, current_end, body)) => {
                    if current_start != start_line || current_end != end_line {
                        relocated_from_line = Some(start_line);
                        (start_line, end_line) = (current_start, current_end);
                    }
                    Some(body)
                }
                None => {
                    stale_span = true;
                    None
                }
            }
        });
        let body_available = body_opt.is_some();
        let body = body_opt.unwrap_or_default();
        let cost = window_cost(&body);
        let may_exempt = exempt_first_symbol && result.symbols.is_empty();
        if let Some(budget) = token_budget
            && !may_exempt
            && used + cost > budget
        {
            result.dropped.push(sym.uid.clone());
            result.truncated = true;
            continue;
        }
        // The first symbol of a fresh call is exempt from the budget so the
        // caller never gets an empty answer — but say so rather than reporting
        // a clean result that silently blew the budget.
        if let Some(budget) = token_budget
            && may_exempt
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
            start_line,
            end_line,
            body,
            body_available,
            is_neighbor,
            stale_span,
            relocated_from_line,
        });
    }

    result
}

/// Read spans using each symbol's recorded repo `local_root` when the caller
/// did not pass an explicit root. An explicit `--root` still uses
/// [`read_symbols`] against that one tree. A repo with no usable
/// `local_root` falls back to `fallback_root` rather than inventing a path.
pub fn read_symbols_from_repo_roots(
    store: &GraphStore,
    specs: &[String],
    neighbors: u8,
    token_budget: Option<usize>,
    fallback_root: &Path,
    limits: crate::index_limits::IndexLimits,
) -> ReadSymbolsResult {
    use std::collections::HashMap;

    use crate::content_reader::FilesystemReader;

    let mut repo_groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut repo_index: HashMap<String, usize> = HashMap::new();
    let mut unresolved: Vec<String> = Vec::new();

    for spec in specs {
        if let Some(repo_uid) = repo_uid_for_spec(store, spec) {
            if let Some(&idx) = repo_index.get(&repo_uid) {
                repo_groups[idx].1.push(spec.clone());
            } else {
                let idx = repo_groups.len();
                repo_index.insert(repo_uid.clone(), idx);
                repo_groups.push((repo_uid, vec![spec.clone()]));
            }
        } else {
            unresolved.push(spec.clone());
        }
    }

    if repo_groups.is_empty() {
        let reader = FilesystemReader::with_limits(fallback_root, limits);
        return read_symbols(store, specs, &reader, neighbors, token_budget);
    }

    let mut merged = ReadSymbolsResult::default();
    merged.not_found.extend(unresolved);
    let mut remaining_budget = token_budget;

    for (repo_uid, group_targets) in &repo_groups {
        let root = repo_local_root(store, repo_uid).unwrap_or_else(|| fallback_root.to_path_buf());
        let reader = FilesystemReader::with_limits(&root, limits);
        // Only the overall first returned window may exceed the budget.
        // Passing the leftover budget into a fresh `read_symbols` would
        // re-apply that exemption for every repo group (nw-542).
        let exempt_first = merged.symbols.is_empty();
        let partial = read_symbols_budgeted(
            store,
            group_targets,
            &reader,
            neighbors,
            remaining_budget,
            exempt_first,
        );
        if let Some(budget) = remaining_budget {
            let used: usize = partial.symbols.iter().map(|s| s.body.len() / 4 + 16).sum();
            remaining_budget = Some(budget.saturating_sub(used));
        }
        merged.symbols.extend(partial.symbols);
        merged.not_found.extend(partial.not_found);
        merged.ambiguous.extend(partial.ambiguous);
        merged.dropped.extend(partial.dropped);
        merged.truncated = merged.truncated || partial.truncated;
        if merged.budget_exceeded_by_first_symbol.is_none() {
            merged.budget_exceeded_by_first_symbol = partial.budget_exceeded_by_first_symbol;
        }
    }
    merged
}

fn repo_uid_for_spec(store: &GraphStore, spec: &str) -> Option<String> {
    resolve(store, spec)
        .into_iter()
        .next()
        .map(|symbol| symbol.repo_uid)
}

/// Resolve `repo_uid`'s recorded `local_root`, filtered to a directory that
/// still exists on disk. `None` covers every reason a caller must fall back:
/// an unknown repo, no recorded root, or a root that no longer exists.
///
/// nw-560: shared by this function's own per-repo-group loop above and by
/// `crate::investigate::resolve_symbol_body_root` (which resolves a single
/// symbol's uid to its repo_uid first, then calls this). Before this was
/// extracted, `investigate.rs` reimplemented the identical
/// lookup→local_root→is_dir chain, which is exactly the kind of duplicate
/// this repo's "sibling gaps" review rule exists to catch.
pub(crate) fn repo_local_root(store: &GraphStore, repo_uid: &str) -> Option<std::path::PathBuf> {
    store
        .lookup_repo(repo_uid)
        .ok()
        .flatten()
        .and_then(|repo| repo.local_root().map(std::path::PathBuf::from))
        .filter(|path| path.is_dir())
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

    #[test]
    fn omitted_root_reads_from_the_repo_local_root_not_cwd() {
        let (_dir, _src, store) = test_repo();
        let elsewhere = tempfile::tempdir().unwrap();
        let res = read_symbols_from_repo_roots(
            &store,
            &["greet".to_string()],
            0,
            None,
            elsewhere.path(),
            crate::index_limits::IndexLimits::default(),
        );
        assert_eq!(res.symbols.len(), 1);
        assert!(
            res.symbols[0].body_available,
            "the recorded repo root must be used when the caller omits --root; got {:?}",
            res.symbols[0]
        );
        assert!(
            res.symbols[0].body.contains("function greet"),
            "body must come from the indexed tree, not the fallback cwd"
        );
    }

    /// nw-542: a token budget spent by the first repo group must drop later
    /// groups rather than re-applying the first-symbol exemption.
    #[test]
    fn a_spent_budget_does_not_exempt_the_first_symbol_of_a_later_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo_a = dir.path().join("a");
        let repo_b = dir.path().join("b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        fs::write(
            repo_a.join("main.js"),
            "function mainA() {\n  return 1;\n}\n",
        )
        .unwrap();
        fs::write(
            repo_b.join("ping.js"),
            "function ping() {\n  return 2;\n}\n",
        )
        .unwrap();

        let (_r, store) =
            index_directory_in_memory(&repo_a, "test", "https://example.com/a", "aaa").unwrap();
        let reader_b = FilesystemReader::new(&repo_b);
        crate::index::index_with_reader(
            &reader_b,
            &store,
            "test",
            "https://example.com/b",
            "bbb",
            Some("b"),
        )
        .unwrap();
        assert!(
            store.list_repos(None).unwrap().len() >= 2,
            "fixture must index two repos so the merge path is exercised"
        );

        let greet_cost = {
            let r = FilesystemReader::new(&repo_a);
            let res = read_symbols(&store, &["mainA".to_string()], &r, 0, None);
            window_cost(&res.symbols[0].body)
        };
        let ping = store
            .lookup_symbols_by_name("ping")
            .unwrap()
            .into_iter()
            .next()
            .expect("ping indexed");

        let res = read_symbols_from_repo_roots(
            &store,
            &["mainA".to_string(), "ping".to_string()],
            0,
            Some(greet_cost),
            dir.path(),
            crate::index_limits::IndexLimits::default(),
        );
        assert_eq!(
            res.symbols.len(),
            1,
            "only the first repo's symbol should fit; got {:?}",
            res.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(res.symbols[0].name, "mainA");
        assert!(
            res.dropped.iter().any(|uid| uid == &ping.uid),
            "the second repo's symbol must be dropped, not exempted: {:?}",
            res.dropped
        );
        assert!(res.truncated);
    }

    /// Write `rel` (with `src`) under a fresh tempdir and index it.
    fn fixture_file(rel: &str, src: &str) -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, src).unwrap();
        let (_r, store) =
            index_directory_in_memory(dir.path(), "test", "https://example.com/r", "sha").unwrap();
        (dir, store)
    }

    fn fixture_with(src: &str) -> (tempfile::TempDir, GraphStore) {
        fixture_file("src/lib.rs", src)
    }

    fn read_one(dir: &tempfile::TempDir, store: &GraphStore, name: &str) -> SymbolWindow {
        let reader = FilesystemReader::new(dir.path());
        let res = read_symbols(store, &[name.to_string()], &reader, 0, None);
        assert_eq!(res.symbols.len(), 1, "{name} must resolve: {res:?}");
        res.symbols.into_iter().next().unwrap()
    }

    /// nw-689: the repo changed since indexing, so the stored span now holds
    /// other code. The unique definition line re-locates the body.
    #[test]
    fn drifted_span_is_relocated_by_unique_definition_line() {
        let (dir, store) = fixture_with("pub fn other() {}\n\npub fn target() {\n    1\n}\n");
        let path = dir.path().join("src/lib.rs");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("// a\n// b\n// c\n{text}")).unwrap();
        let w = read_one(&dir, &store, "target");
        assert!(w.body.contains("pub fn target()"), "{w:?}");
        assert_eq!(w.relocated_from_line, Some(3));
        assert_eq!(w.start_line, 6, "the window moved to the definition: {w:?}");
        assert!(!w.stale_span);
        assert!(w.body_available);
    }

    /// A short insertion can leave the original name inside the old window.
    /// The declaration moved even though a name-only check would accept it.
    #[test]
    fn short_drift_relocates_even_when_name_remains_in_window() {
        let (dir, store) = fixture_with("pub fn other() {}\n\npub fn target() {\n    1\n}\n");
        let path = dir.path().join("src/lib.rs");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("// inserted\n{text}")).unwrap();
        let w = read_one(&dir, &store, "target");
        assert_eq!(w.relocated_from_line, Some(3), "{w:?}");
        assert_eq!(w.start_line, 4, "{w:?}");
        assert!(w.body.starts_with("pub fn target()"), "{w:?}");
    }

    #[test]
    fn repeated_annotation_with_a_call_does_not_validate_the_wrong_method() {
        let src =
            "class Api {\n    @Deprecated\n    public void target() {\n        work();\n    }\n}\n";
        let (dir, store) = fixture_file("src/Api.java", src);
        fs::write(
            dir.path().join("src/Api.java"),
            "class Api {\n    @Deprecated\n    public void other() {\n        target();\n    }\n    @Deprecated\n    public void target() {\n        work();\n    }\n}\n",
        )
        .unwrap();
        let w = read_one(&dir, &store, "target");
        assert!(!w.body.contains("void other()"), "{w:?}");
        assert!(w.stale_span || w.body.contains("void target()"), "{w:?}");
    }

    #[test]
    fn relocated_shorter_definition_does_not_include_the_next_function() {
        let (dir, store) = fixture_with("pub fn target() {\n    1\n}\n");
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn other() {}\npub fn target() {}\npub fn after() {}\n",
        )
        .unwrap();
        let w = read_one(&dir, &store, "target");
        assert_eq!(w.body, "pub fn target() {}", "{w:?}");
        assert_eq!((w.start_line, w.end_line), (2, 2), "{w:?}");
    }

    #[test]
    fn relocated_longer_definition_keeps_its_end() {
        let (dir, store) = fixture_with("pub fn target() {\n    1\n}\n");
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn other() {}\npub fn target() {\n    let x = 1;\n    let y = 2;\n    x + y\n}\n",
        )
        .unwrap();
        let w = read_one(&dir, &store, "target");
        assert!(w.body.ends_with("}"), "{w:?}");
        assert!(w.body.contains("x + y"), "{w:?}");
        assert_eq!((w.start_line, w.end_line), (2, 6), "{w:?}");
    }

    #[test]
    fn edited_one_line_body_keeps_the_same_definition_available() {
        let (dir, store) = fixture_with("pub fn target() { 1 }\n");
        fs::write(dir.path().join("src/lib.rs"), "pub fn target() { 2 }\n").unwrap();
        let w = read_one(&dir, &store, "target");
        assert_eq!(w.body, "pub fn target() { 2 }", "{w:?}");
        assert!(w.body_available && !w.stale_span, "{w:?}");
    }

    /// nw-689: no unique definition to re-locate to -> say the span is stale
    /// rather than returning another symbol's body as this one's.
    #[test]
    fn drifted_span_without_a_unique_match_is_stale_not_wrong() {
        let (dir, store) = fixture_with("pub fn target() {\n    1\n}\n");
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn a() {}\nfn b() {}\nfn c() {}\n",
        )
        .unwrap();
        let w = read_one(&dir, &store, "target");
        assert!(
            w.stale_span && !w.body_available && w.body.is_empty(),
            "{w:?}"
        );
        assert!(w.relocated_from_line.is_none());
    }

    /// A CALL naming the symbol is not a definition: relocating to
    /// `let x = target();` would return exactly the wrong body nw-689 is about.
    #[test]
    fn a_call_site_is_not_a_relocation_target() {
        let (dir, store) = fixture_with("pub fn target() {\n    1\n}\n");
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\nfn f() {\n    let x = target();\n}\n",
        )
        .unwrap();
        let w = read_one(&dir, &store, "target");
        assert!(w.stale_span && w.body.is_empty(), "{w:?}");
    }

    /// Counterweight: an in-place span is returned untouched.
    #[test]
    fn fresh_span_is_unchanged() {
        let (dir, store) = fixture_with("pub fn target() {\n    1\n}\n");
        let w = read_one(&dir, &store, "target");
        assert!(
            w.body_available && !w.stale_span && w.relocated_from_line.is_none(),
            "{w:?}"
        );
        assert!(w.body.contains("pub fn target()"));
    }

    /// Counterweight: Java folds annotations INTO the method node, so a
    /// fresh span can start with more than five annotation lines before the
    /// name. That is not drift.
    #[test]
    fn a_long_annotation_prefix_is_not_drift() {
        let src = "class Api {\n    @GetMapping(\n        value = \"/x\",\n        produces = \"a\",\n        consumes = \"b\"\n    )\n    @Deprecated\n    public String handleThing() {\n        return \"\";\n    }\n}\n";
        let (dir, store) = fixture_file("src/Api.java", src);
        let w = read_one(&dir, &store, "handleThing");
        assert!(
            !w.body.lines().take(5).any(|l| l.contains("handleThing")),
            "fixture must put the name past line 5 of the span to test anything: {w:?}"
        );
        assert!(w.body_available && !w.stale_span, "{w:?}");
        assert!(w.relocated_from_line.is_none(), "{w:?}");
    }

    /// Counterweight: a Svelte component symbol is named after the FILE and
    /// spans the whole file; its name never appears in the source.
    #[test]
    fn a_file_named_component_is_not_drift() {
        let src = "<script>\n  let count = 0;\n</script>\n\n<button on:click={() => count++}>\n  {count}\n</button>\n";
        let (dir, store) = fixture_file("src/Counter.svelte", src);
        let w = read_one(&dir, &store, "Counter");
        assert!(w.body_available && !w.stale_span, "{w:?}");
    }

    #[test]
    fn vue_options_api_component_is_not_drift() {
        let src = "<script>\nexport default {\n  data() { return { count: 0 }; }\n}\n</script>\n";
        let (dir, store) = fixture_file("src/Counter.vue", src);
        let w = read_one(&dir, &store, "Counter");
        assert!(w.body_available && !w.stale_span, "{w:?}");
        assert!(w.body.starts_with("export default"), "{w:?}");
    }

    #[test]
    fn go_const_group_member_is_not_drift() {
        let src = "package p\nconst (\n    Answer = 42\n    Other = 9\n)\n";
        let (dir, store) = fixture_file("src/values.go", src);
        let w = read_one(&dir, &store, "Answer");
        assert!(w.body_available && !w.stale_span, "{w:?}");
    }

    #[test]
    fn span_holds_symbol_accepts_legitimate_shapes() {
        // Name on the last line (C `typedef struct { ... } Name;`).
        let typedef = "typedef struct {\n int a;\n int b;\n int c;\n int d;\n int e;\n} Point;";
        assert!(span_holds_symbol(
            typedef,
            "Point",
            "src/p.h",
            "typedef struct {"
        ));
        // Whole-word only: `target_x` does not name `target`.
        assert!(!span_holds_symbol(
            "fn target_x() {}",
            "target",
            "src/lib.rs",
            "fn target() {}"
        ));
        // Non-identifier names (test blocks, FQNs) are never checked.
        assert!(span_holds_symbol(
            "fn a() {}",
            "renders the list",
            "src/a.ts",
            ""
        ));
        // Go receiver method.
        assert!(span_holds_symbol(
            "func (s *Server) Handle(w http.ResponseWriter) {\n}",
            "Handle",
            "srv.go",
            "func (s *Server) Handle(w http.ResponseWriter) {"
        ));
    }

    #[test]
    fn relocate_requires_a_definition_keyword_before_the_name() {
        assert_eq!(relocate("x\nlet y = target();\n", "target", 3), None);
        assert_eq!(
            relocate("x\npub async fn target() {\n}\n", "target", 3),
            Some((2, 4))
        );
        assert_eq!(
            relocate("func (s *S) Target() {\n}\n", "Target", 2),
            Some((1, 2))
        );
        // Two definitions -> ambiguous -> no guess.
        assert_eq!(
            relocate("fn target() {}\nfn target() {}\n", "target", 1),
            None
        );
    }
}
