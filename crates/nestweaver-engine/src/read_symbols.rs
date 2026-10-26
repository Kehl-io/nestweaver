//! Symbol-window reads (RFC F5).
//!
//! Returns just a symbol's source span (`start_line..=end_line`, from P0.1's
//! `Symbol.end_line`) instead of a whole file — the cheapest token cut in the
//! agent loop. Optionally includes adjacent symbols in the same file
//! (`neighbors`) and is token-budget aware. Comment stripping is a planned
//! follow-up (default-off; deferred to avoid false-elision risk).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nestweaver_schema::{Language, Symbol};
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
    /// selected repository root, or an out-of-range span) — the body is then
    /// an empty string that would otherwise be indistinguishable from a genuinely
    /// empty symbol. Re-index a moved checkout or pass its explicit `root`.
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
    /// nw-804: the body alone was larger than `token_budget`, so only its
    /// first lines are returned, followed by a marker line naming what was
    /// cut. `start_line`/`end_line` still give the whole symbol's span.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_truncated: bool,
}

/// A spec that matched more than one symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousMatch {
    pub query: String,
    pub candidate_uids: Vec<String>,
    /// nw-113: every candidate with its display name and file, in the same
    /// order as `candidate_uids`, so a caller can pick one (or qualify the
    /// name as `path::name`) without a second lookup. `serde(default)` keeps
    /// an older daemon's response readable.
    #[serde(default)]
    pub candidates: Vec<AmbiguousCandidate>,
    /// nw-113: how to pick one candidate, fitted to the query: a UID always
    /// works; `path::name` is offered only when the caller did not already
    /// qualify the name and the candidates are in different files, and
    /// `path::Class::name` when each candidate has its own class. Empty from
    /// an older daemon.
    #[serde(default)]
    pub hint: String,
}

/// nw-113: a `read-symbols` target whose qualifier named no file and no
/// container, so it was resolved by its last segment alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualifierIgnored {
    pub query: String,
    pub note: String,
}

/// One candidate of an [`AmbiguousMatch`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousCandidate {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub start_line: u32,
}

impl From<&Symbol> for AmbiguousCandidate {
    fn from(symbol: &Symbol) -> Self {
        Self {
            uid: symbol.uid.clone(),
            name: symbol.display_name().into_owned(),
            kind: symbol.kind.to_string(),
            file_path: symbol.file_path.clone(),
            start_line: symbol.start_line,
        }
    }
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
    /// Set when the FIRST symbol alone exceeded `token_budget` even after it
    /// was cut to fit (nw-804: only a budget smaller than the truncation
    /// marker, or a budget of 0, which returns the first symbol whole).
    ///
    /// One symbol is always returned, because an empty answer to "read this
    /// symbol" is useless. That guarantee is deliberate — but it was kept
    /// silently, with `truncated: false`, so a caller asking for 1 token
    /// received ~6,700 and had no way to know the budget had been ignored
    /// (nw-111). The guarantee stays; the silence does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_exceeded_by_first_symbol: Option<BudgetOverrun>,
    /// nw-113: targets whose leading qualifier named nothing and were
    /// resolved by a shorter qualified reading that still names something
    /// (`zzz::Svc::run` read as `Svc::run`). A qualifier is never dropped
    /// down to the bare name. Absent when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub qualifier_ignored: Vec<QualifierIgnored>,
    /// 12.1.1 (B1/M1): `path::name` suggestions for the `not_found` targets,
    /// from the same builder `symbol` and `impact` use, bounded to
    /// [`crate::did_you_mean::DID_YOU_MEAN_LIMIT`]. Absent when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub did_you_mean: Vec<String>,
    /// QA #11: the same suggestions PER not-found target, each bounded to
    /// [`crate::did_you_mean::DID_YOU_MEAN_LIMIT`]. The flat `did_you_mean`
    /// above is one list capped at five across every miss, so a read with
    /// several misses could not say which suggestion answered which target
    /// and lost every target after the first few. Absent when empty.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub did_you_mean_by_target: std::collections::BTreeMap<String, Vec<String>>,
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
/// nw-113: the line placed between runs of top-level code when reading a
/// `<module>` owner, where function, class and method bodies were skipped.
pub const ELISION: &str = "⋮";

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
    let objective_c =
        nestweaver_parser::detect_language(Path::new(file_path)) == Some(Language::ObjectiveC);
    let decl = declaration_start(&lines, objective_c);
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
fn declaration_start(lines: &[&str], objective_c: bool) -> usize {
    let mut depth: i32 = 0;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let is_comment = t.starts_with("//") || t.starts_with("/*") || t.starts_with('*');
        // Objective-C uses @ for declarations as well as directives. These
        // headers name the symbol itself, unlike Java/Dart annotations. Keep
        // the distinction language- and token-aware so decorators still skip.
        let is_objc_declaration = objective_c
            && ["@protocol", "@interface", "@implementation"]
                .iter()
                .any(|&keyword| {
                    t.strip_prefix(keyword)
                        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
                });
        let is_prefix = depth > 0
            || t.is_empty()
            || is_comment
            || (t.starts_with('@') && !is_objc_declaration)
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

/// nw-113: the current top-level code of a file whose parse still has a
/// `<module>` owner: the lines outside every code-bearing span, as runs
/// joined by [`ELISION`]. Whitespace-only runs are dropped. `None` when the
/// file no longer has top-level calls (the owner is gone), which reads as a
/// stale span like any other vanished symbol.
fn module_owner_body(text: &str, parsed: &nestweaver_parser::ParsedFile) -> Option<(u32, String)> {
    let owner = parsed.symbols.iter().find(|raw| raw.is_module_owner())?;
    let lines: Vec<&str> = text.lines().collect();
    let mut covered = vec![false; lines.len()];
    for raw in parsed
        .symbols
        .iter()
        .filter(|raw| raw.kind.contains_code() && !raw.is_module_owner())
    {
        let start = (raw.start_line.max(1) as usize - 1).min(lines.len());
        let end = (raw.end_line.max(raw.start_line) as usize).min(lines.len());
        covered[start..end].iter_mut().for_each(|line| *line = true);
    }
    let mut runs: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for (line, is_covered) in lines.iter().zip(&covered) {
        if *is_covered {
            runs.push(std::mem::take(&mut current));
        } else {
            current.push(line);
        }
    }
    runs.push(current);
    let runs: Vec<String> = runs
        .into_iter()
        .filter(|run| run.iter().any(|line| !line.trim().is_empty()))
        .map(|run| run.join("\n").trim_matches('\n').to_string())
        .collect();
    Some((owner.end_line, runs.join(&format!("\n{ELISION}\n"))))
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

/// Resolve a spec (`sym:` UID, bare name, `file::name`,
/// `file::Class::method`, `Class::method`, or a dotted FQN) to candidate
/// symbols.
///
/// nw-113: a qualified spec resolves through the strict
/// [`GraphStore::lookup_symbols_by_spec`], so a file and container narrow
/// together. 12.1.1 (B1/M1): a qualifier that names nothing is not found,
/// the same as `symbol` and `impact`; it is never dropped down to the bare
/// name. [`resolve_spec_detailed`] reports the one shorter reading kept.
pub fn resolve_spec(store: &GraphStore, spec: &str) -> Vec<Symbol> {
    resolve_spec_detailed(store, spec).symbols
}

/// What [`resolve_spec_detailed`] found for one spec.
#[derive(Debug, Default)]
pub struct SpecResolution {
    pub symbols: Vec<Symbol>,
    /// The leading qualifiers named nothing and a shorter QUALIFIED reading
    /// answered instead: `(dropped qualifiers, reading that answered)`, e.g.
    /// `("zzz", "Svc::run")`. Never a bare name.
    pub qualifier_ignored: Option<(String, String)>,
}

/// [`resolve_spec`], also saying whether a leading qualifier was dropped.
///
/// nw-113 final review Minor 4: when the full spec names nothing, the
/// shorter readings that are still qualified are tried outermost qualifier
/// first (`zzz::Svc::run`, then `Svc::run`), so a Rust module path such as
/// `net::inner::send` keeps resolving as `inner::send`. 12.1.1 (B1/M1): that
/// is where it stops. The last segment alone is never tried, and a
/// `path::<module>` is never shortened at all, because an owner is
/// meaningless without the file that names it.
pub fn resolve_spec_detailed(store: &GraphStore, spec: &str) -> SpecResolution {
    if spec.starts_with("sym:") {
        return SpecResolution {
            symbols: store.lookup_symbol(spec).ok().into_iter().collect(),
            qualifier_ignored: None,
        };
    }
    if let Some(qualified) = nestweaver_schema::QualifiedSpec::parse(spec) {
        let strict = store.lookup_symbols_by_spec(spec).unwrap_or_default();
        if !strict.is_empty() || qualified.name == nestweaver_schema::MODULE_OWNER_NAME {
            return SpecResolution {
                symbols: strict,
                qualifier_ignored: None,
            };
        }
        // I5: a leading qualifier is dropped only when it cannot be a file.
        // `nothere.ts::Svc::run` names a file that does not exist; answering
        // it with another file's `Svc::run` reads the wrong code. It is not
        // found, and `did_you_mean` offers the shorter reading first.
        let segments: Vec<&str> = spec.split("::").collect();
        if segments.len() < 3 || qualifier_may_name_a_file(store, None, segments[0]) {
            return SpecResolution::default();
        }
        if let Some((drop, reading, symbols)) = shorter_qualified_reading(store, &segments) {
            return SpecResolution {
                symbols,
                qualifier_ignored: Some((segments[..drop].join("::"), reading)),
            };
        }
        return SpecResolution::default();
    }
    // QA #5: `UUID.String` named every `String` in the graph. A dotted
    // `Type.member` is read as `Type::member` first; only when that names
    // nothing does the last segment answer, as before.
    if let Some((owner, member)) = spec.rsplit_once('.')
        && !owner.is_empty()
        && !member.is_empty()
    {
        let qualified = store
            .lookup_symbols_by_spec(&format!("{owner}::{member}"))
            .unwrap_or_default();
        if !qualified.is_empty() {
            return SpecResolution {
                symbols: qualified,
                qualifier_ignored: None,
            };
        }
    }
    SpecResolution {
        symbols: store
            .lookup_symbols_by_name(last_dotted_segment(spec))
            .unwrap_or_default(),
        qualifier_ignored: None,
    }
}

/// I5: whether a spec's leading qualifier could name a file: it has a path
/// separator or an extension, or some indexed file has it as its name or
/// stem. Such a qualifier is never dropped. A bare word that names no file
/// (a Rust module path such as `net::inner::send`) can be.
fn qualifier_may_name_a_file(
    store: &GraphStore,
    table: Option<&nestweaver_store::SymbolNameTable>,
    qualifier: &str,
) -> bool {
    if qualifier.contains(['/', '\\', '.']) {
        return true;
    }
    let is_file = |_: &str, symbol: &Symbol| {
        nestweaver_schema::file_qualifier_rank(qualifier, &symbol.file_path).map(|_| ())
    };
    match table {
        Some(table) => !table.scan(is_file).is_empty(),
        // Best effort: a failed lookup keeps the qualifier, the safe side.
        None => store
            .scan_symbol_names(is_file)
            .map_or(true, |found| !found.is_empty()),
    }
}

/// The first shorter reading of `segments` that is still qualified and names
/// something, outermost qualifier first: `(segments dropped, reading,
/// symbols)`.
fn shorter_qualified_reading(
    store: &GraphStore,
    segments: &[&str],
) -> Option<(usize, String, Vec<Symbol>)> {
    (1..segments.len().saturating_sub(1)).find_map(|drop| {
        let reading = segments[drop..].join("::");
        let symbols = store.lookup_symbols_by_spec(&reading).unwrap_or_default();
        (!symbols.is_empty()).then_some((drop, reading, symbols))
    })
}

/// 12.1.1 (B1/M1): `did_you_mean` for the `not_found` specs of a read, merged
/// in spec order, deduplicated and bounded like every other miss. Best
/// effort: a lookup failure is logged and suggests nothing.
pub fn not_found_suggestions(store: &GraphStore, not_found: &[String]) -> Vec<String> {
    flatten_suggestions(
        not_found,
        &not_found_suggestions_by_target(store, not_found),
    )
}

/// QA #11: [`not_found_suggestions`] kept per target, each list bounded to
/// [`crate::did_you_mean::DID_YOU_MEAN_LIMIT`]. A target with no suggestion
/// is absent. One symbol-table fetch serves every target.
pub fn not_found_suggestions_by_target(
    store: &GraphStore,
    not_found: &[String],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut by_target = std::collections::BTreeMap::new();
    // I4: one symbol-table fetch for every not-found spec of the read.
    let mut table = None;
    for spec in not_found {
        if spec.starts_with("sym:") || by_target.contains_key(spec) {
            continue;
        }
        let mut found = crate::did_you_mean::did_you_mean_candidates_with_table(
            store,
            &mut table,
            spec,
            |_| true,
        )
        .unwrap_or_else(|error| {
            tracing::warn!("did_you_mean_candidates lookup failed for '{spec}': {error:#}");
            Vec::new()
        });
        // I5: a file-looking qualifier that names nothing is not dropped, so
        // the shorter reading that does answer is offered first.
        if nestweaver_schema::QualifiedSpec::parse(spec).is_some() {
            let segments: Vec<&str> = spec.split("::").collect();
            if qualifier_may_name_a_file(store, table.as_ref(), segments[0])
                && let Some((_, reading, _)) = shorter_qualified_reading(store, &segments)
            {
                found.retain(|name| *name != reading);
                found.insert(0, reading);
            }
        }
        let mut names: Vec<String> = Vec::new();
        for name in found {
            if names.len() == crate::did_you_mean::DID_YOU_MEAN_LIMIT {
                break;
            }
            if !names.contains(&name) {
                names.push(name);
            }
        }
        if !names.is_empty() {
            by_target.insert(spec.clone(), names);
        }
    }
    by_target
}

/// The flat, spec-ordered, deduplicated and bounded `did_you_mean` list.
fn flatten_suggestions(
    not_found: &[String],
    by_target: &std::collections::BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for spec in not_found {
        for name in by_target.get(spec).into_iter().flatten() {
            if names.len() == crate::did_you_mean::DID_YOU_MEAN_LIMIT {
                return names;
            }
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names
}

/// Fill both suggestion fields of `result` from its `not_found` targets.
pub fn attach_not_found_suggestions(store: &GraphStore, result: &mut ReadSymbolsResult) {
    let by_target = not_found_suggestions_by_target(store, &result.not_found);
    result.did_you_mean = flatten_suggestions(&result.not_found, &by_target);
    result.did_you_mean_by_target = by_target;
}

/// nw-113: the [`AmbiguousMatch::hint`] for `candidates` of `spec`.
fn ambiguity_hint(store: &GraphStore, spec: &str, candidates: &[Symbol]) -> String {
    let containers = store
        .symbol_container_chains(candidates, 1)
        .unwrap_or_default();
    let class_of = |s: &Symbol| containers.get(&s.uid).and_then(|chain| chain.first());
    // Candidates in a class are told apart by `path::Class::name`.
    let mut seen = HashSet::new();
    let classes_distinct = candidates
        .iter()
        .filter_map(|s| class_of(s).map(|class| (s.file_path.as_str(), class.as_str())))
        .all(|pair| seen.insert(pair));
    let classed = candidates.iter().filter(|s| class_of(s).is_some()).count();
    if classes_distinct && classed == candidates.len() {
        return "pass a UID or path::Class::name".to_string();
    }
    // `path::name` only helps when it names one candidate each: every
    // candidate in its own file, and not already the file the caller named.
    let mut files = HashSet::new();
    let files_distinct = candidates
        .iter()
        .all(|s| files.insert(s.file_path.as_str()));
    if files_distinct && nestweaver_schema::QualifiedSpec::parse(spec).is_none() {
        return "pass a UID or path::name".to_string();
    }
    // nw-113 final review Minor 4: same-file candidates, or a `path::name`
    // that would still match two, are told apart by their class where they
    // have one, and otherwise only by UID.
    if classed > 0 && classes_distinct {
        return "pass a UID or path::Class::name".to_string();
    }
    "pass a UID".to_string()
}

/// Dotted FQN forms (`pkg.mod.func`): the last segment is the symbol name.
fn last_dotted_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Estimate the token cost of a window (chars/4 + small metadata overhead).
fn window_cost(body: &str) -> usize {
    body.len() / 4 + 16
}

/// nw-804: the leading whole lines of `body` that fit `budget` (by
/// [`window_cost`]) together with a closing marker line naming the cut. A
/// first line too long to fit (minified code) is cut at a char boundary. The
/// marker is always kept, so a budget smaller than the marker alone still
/// overruns and is reported as such by the caller.
fn truncate_to_budget(body: &str, budget: usize) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();
    let allowance = budget.saturating_sub(window_cost("")) * 4;
    let marker = |kept: usize| {
        format!(
            "{ELISION} truncated: {} of {total} lines omitted to fit the {budget}-token \
             budget; raise --token-budget for the whole body",
            total - kept
        )
    };
    let mut kept = String::new();
    let mut kept_lines = 0;
    for line in &lines {
        let next = kept.len() + line.len() + 1;
        if next + marker(kept_lines + 1).len() > allowance {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
        kept_lines += 1;
    }
    if kept_lines == 0
        && let Some(first) = lines.first()
    {
        let room = allowance.saturating_sub(marker(0).len() + 1);
        let mut end = room.min(first.len());
        while !first.is_char_boundary(end) {
            end -= 1;
        }
        if end > 0 {
            kept.push_str(&first[..end]);
            kept.push('\n');
        }
    }
    kept.push_str(&marker(kept_lines));
    kept
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
        let SpecResolution {
            symbols: candidates,
            qualifier_ignored,
        } = resolve_spec_detailed(store, spec);
        if let Some((qualifier, name)) = qualifier_ignored {
            result.qualifier_ignored.push(QualifierIgnored {
                query: spec.clone(),
                note: format!(
                    "qualifier '{qualifier}' names no file or container of '{name}'; \
                     matched by '{name}' alone"
                ),
            });
        }
        match candidates.len() {
            0 => result.not_found.push(spec.clone()),
            1 => primary.push(candidates.into_iter().next().expect("len == 1")),
            _ => result.ambiguous.push(AmbiguousMatch {
                query: spec.clone(),
                candidate_uids: candidates.iter().map(|s| s.uid.clone()).collect(),
                candidates: candidates.iter().map(AmbiguousCandidate::from).collect(),
                hint: ambiguity_hint(store, spec, &candidates),
            }),
        }
    }

    attach_not_found_suggestions(store, &mut result);

    // 2. Expand with neighbours (adjacent symbols in the same file), de-duped.
    let mut ordered: Vec<(Symbol, bool)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for sym in primary {
        if neighbors > 0 {
            let mut in_file = store
                .symbols_in_file_in_repo(&sym.file_path, &sym.repo_uid)
                .unwrap_or_default();
            // nw-113: the whole-file `<module>` owner is never a neighbour.
            in_file.retain(|s| !s.is_module_owner() || s.uid == sym.uid);
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
            if sym.is_module_owner() {
                let owner = parsed
                    .as_ref()
                    .and_then(|parsed| module_owner_body(text, parsed));
                // The owner always spans the whole file, so a changed line
                // count is not a relocation: follow the file's end only.
                if let Some((current_end, _)) = owner {
                    end_line = current_end;
                }
                stale_span = owner.is_none();
                return owner.map(|(_, body)| body);
            }
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
        let mut body = body_opt.unwrap_or_default();
        let mut cost = window_cost(&body);
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
        // caller never gets an empty answer. nw-804: it is cut to fit rather
        // than returned whole, and the cut is marked in the body and flagged.
        // A budget of 0 keeps its documented meaning, "only the first symbol",
        // and returns it whole.
        let mut body_truncated = false;
        if let Some(budget) = token_budget
            && may_exempt
            && cost > budget
        {
            if budget > 0 {
                body = truncate_to_budget(&body, budget);
                cost = window_cost(&body);
                body_truncated = true;
            }
            result.truncated = true;
            // Only a budget too small for even the cut window still overruns;
            // that is said, never silent (nw-111).
            if cost > budget {
                result.budget_exceeded_by_first_symbol = Some(BudgetOverrun {
                    requested_tokens: budget,
                    returned_tokens: cost,
                    note: format!(
                        "the first symbol{} still costs ~{cost} tokens, over the requested \
                         budget of {budget}; it is returned because an empty result answers \
                         nothing, so this response EXCEEDS the budget",
                        if body_truncated {
                            ", cut to its truncation marker,"
                        } else {
                            ""
                        }
                    ),
                });
            }
        }
        used += cost;
        result.symbols.push(SymbolWindow {
            name: sym.display_name().into_owned(),
            uid: sym.uid,
            kind: sym.kind.to_string(),
            path: sym.file_path,
            start_line,
            end_line,
            body,
            body_available,
            is_neighbor,
            stale_span,
            relocated_from_line,
            body_truncated,
        });
    }

    result
}

/// Read spans using each symbol's recorded repo `local_root` when the caller
/// did not pass an explicit root. An explicit `--root` still uses
/// [`read_symbols`] against that one tree. A repo with no usable
/// `local_root` reports its body unavailable; it never reads the process cwd.
/// The legacy fallback argument is retained for source compatibility.
pub fn read_symbols_from_repo_roots(
    store: &GraphStore,
    specs: &[String],
    neighbors: u8,
    token_budget: Option<usize>,
    _fallback_root: &Path,
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
        let reader = FilesystemReader::with_limits(Path::new(""), limits);
        return read_symbols(store, specs, &reader, neighbors, token_budget);
    }

    let mut merged = ReadSymbolsResult::default();
    merged.not_found.extend(unresolved);
    let mut remaining_budget = token_budget;

    for (repo_uid, group_targets) in &repo_groups {
        let root = repo_local_root(store, repo_uid).unwrap_or_default();
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
        merged.qualifier_ignored.extend(partial.qualifier_ignored);
        merged.dropped.extend(partial.dropped);
        merged.truncated = merged.truncated || partial.truncated;
        if merged.budget_exceeded_by_first_symbol.is_none() {
            merged.budget_exceeded_by_first_symbol = partial.budget_exceeded_by_first_symbol;
        }
    }
    attach_not_found_suggestions(store, &mut merged);
    merged
}

fn repo_uid_for_spec(store: &GraphStore, spec: &str) -> Option<String> {
    resolve_spec(store, spec)
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

    #[test]
    fn neighbors_never_cross_repositories_with_the_same_paths_names_and_spans() {
        let dir = tempfile::tempdir().unwrap();
        let merged = GraphStore::in_memory().unwrap();
        let mut seeds = Vec::new();
        for (name, canary) in [("one", "first-repo-canary"), ("two", "second-repo-canary")] {
            let root = dir.path().join(name);
            fs::create_dir_all(&root).unwrap();
            let source = format!(
                "function before() {{ return '{canary}'; }}\nfunction middle() {{ return '{canary}'; }}\nfunction after() {{ return '{canary}'; }}\n"
            );
            fs::write(root.join("main.js"), source).unwrap();
            let (_, store) = index_directory_in_memory(
                &root,
                name,
                &format!("https://example.com/{name}"),
                "sha",
            )
            .unwrap();
            for repo in store.list_repos(None).unwrap() {
                merged.insert_repo(&repo).unwrap();
            }
            for symbol in store.list_all_symbols().unwrap() {
                if symbol.name == "middle" {
                    seeds.push((symbol.uid.clone(), symbol.repo_uid.clone(), canary));
                }
                merged.insert_symbol(&symbol).unwrap();
            }
        }
        assert_eq!(seeds.len(), 2);
        for (uid, repo_uid, canary) in seeds {
            let result = read_symbols_from_repo_roots(
                &merged,
                &[uid],
                2,
                None,
                dir.path(),
                crate::index_limits::IndexLimits::default(),
            );
            assert_eq!(result.symbols.len(), 3);
            for window in result.symbols {
                assert!(window.body.contains(canary));
                assert_eq!(
                    merged.lookup_symbol(&window.uid).unwrap().repo_uid,
                    repo_uid
                );
            }
        }
    }

    #[test]
    fn missing_owning_root_never_uses_a_same_path_fallback() {
        let (_dir, src, store) = test_repo();
        let wrong = tempfile::tempdir().unwrap();
        fs::write(
            wrong.path().join("main.js"),
            "function greet(name) { return 'wrong-root-canary'; }\n",
        )
        .unwrap();
        fs::rename(&src, src.with_extension("moved")).unwrap();
        let result = read_symbols_from_repo_roots(
            &store,
            &["greet".into()],
            0,
            None,
            wrong.path(),
            crate::index_limits::IndexLimits::default(),
        );
        assert_eq!(result.symbols.len(), 1);
        assert!(!result.symbols[0].body_available);
        assert!(!result.symbols[0].body.contains("wrong-root-canary"));
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

    /// Write every `(rel, src)` under a fresh tempdir and index it.
    fn fixture_files(files: &[(&str, &str)]) -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        for (rel, src) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, src).unwrap();
        }
        let (_r, store) =
            index_directory_in_memory(dir.path(), "test", "https://example.com/r", "sha").unwrap();
        (dir, store)
    }

    /// nw-113: `path::<module>` reads that file's owner, `file::name` narrows
    /// an ordinary name by file, and an ambiguous suffix is still reported,
    /// now with every candidate's path.
    #[test]
    fn file_qualifier_narrows_owner_and_symbol_lookups() {
        let (dir, store) = fixture_files(&[
            (
                "index.js",
                "function app() { return 1; }
app();
",
            ),
            (
                "src/controllers/index.js",
                "function app() { return 2; }
app();
",
            ),
            ("src/wsgi.py", "def app():\n    return 3\n\napp()\n"),
            (
                "src/lib.js",
                "function only() { return 4; }\nmodule.exports = { only };\n",
            ),
        ]);
        let reader = FilesystemReader::new(dir.path());
        let read = |spec: &str| read_symbols(&store, &[spec.to_string()], &reader, 0, None);

        let res = read("wsgi.py::<module>");
        assert_eq!(res.symbols.len(), 1, "{res:?}");
        assert_eq!(res.symbols[0].path, "src/wsgi.py");
        assert_eq!(res.symbols[0].name, "src/wsgi.py::<module>");

        let res = read("src/controllers/index.js::<module>");
        assert_eq!(res.symbols.len(), 1, "{res:?}");
        assert_eq!(res.symbols[0].path, "src/controllers/index.js");

        let res = read("index.js::<module>");
        assert_eq!(res.symbols.len(), 1, "an exact path wins: {res:?}");
        assert_eq!(res.symbols[0].path, "index.js");

        let res = read("controllers/index.js::app");
        assert_eq!(res.symbols.len(), 1, "{res:?}");
        assert_eq!(res.symbols[0].path, "src/controllers/index.js");

        let res = read("wsgi::app");
        assert_eq!(res.symbols.len(), 1, "{res:?}");
        assert_eq!(res.symbols[0].path, "src/wsgi.py");

        // 12.1.1 (B1/M1): a file with no top-level calls has no owner, so
        // its `path::<module>` is not found. It never widens to every owner,
        // and the suggestions never list other files' owners.
        for spec in ["src/lib.js::<module>", "nope/missing.js::<module>"] {
            let res = read(spec);
            assert!(res.symbols.is_empty(), "{spec}: {res:?}");
            assert!(res.ambiguous.is_empty(), "{spec}: {res:?}");
            assert_eq!(res.not_found, [spec], "{res:?}");
            assert!(
                res.did_you_mean.iter().all(|s| !s.ends_with("::<module>")),
                "{spec}: another file's owner is no suggestion: {res:?}"
            );
        }
        let res = read("nope/index.js::<module>");
        assert_eq!(res.not_found, ["nope/index.js::<module>"], "{res:?}");
        let mut suggested = res.did_you_mean.clone();
        suggested.sort();
        assert_eq!(
            suggested,
            ["index.js::<module>", "src/controllers/index.js::<module>"],
            "owners of a file of the same name: {res:?}"
        );

        let res = read("<module>");
        assert!(res.symbols.is_empty(), "{res:?}");
        let [ambiguous] = res.ambiguous.as_slice() else {
            panic!("{res:?}")
        };
        let mut paths: Vec<&str> = ambiguous
            .candidates
            .iter()
            .map(|c| c.file_path.as_str())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            ["index.js", "src/controllers/index.js", "src/wsgi.py"]
        );
        assert!(
            ambiguous
                .candidates
                .iter()
                .all(|c| c.name == format!("{}::<module>", c.file_path)),
            "{ambiguous:?}"
        );
        assert_eq!(ambiguous.candidate_uids.len(), 3);
    }

    /// nw-113 final round: `path::Class::method` narrows by file and class;
    /// the ambiguity hint never asks for the `path::name` the caller already
    /// passed, nor for one that cannot help.
    ///
    /// 12.1.1 (B1/M1): a qualifier that names no file or container is not
    /// found, with `path::name` suggestions, the same as `symbol` and
    /// `impact`. It is never widened to the last segment.
    /// QA #11: several misses in one read each keep their own suggestions;
    /// the flat `did_you_mean` (one list capped at five) is unchanged.
    #[test]
    fn did_you_mean_is_reported_per_not_found_target() {
        let (dir, store) = fixture_files(&[(
            "b/svc.ts",
            "export class Other { run() { return 3; } }\n\
             export function strip(b: string) { return b; }\n",
        )]);
        let reader = FilesystemReader::new(dir.path());
        let specs = [
            "nothere.ts::Other".to_string(),
            "nothere.ts::strip".to_string(),
        ];
        let res = read_symbols(&store, &specs, &reader, 0, None);
        assert_eq!(res.not_found, specs, "{res:?}");
        assert_eq!(
            res.did_you_mean_by_target
                .get("nothere.ts::Other")
                .map(Vec::as_slice),
            Some(&["b/svc.ts::Other".to_string()][..]),
            "{res:?}"
        );
        assert_eq!(
            res.did_you_mean_by_target
                .get("nothere.ts::strip")
                .map(Vec::as_slice),
            Some(&["b/svc.ts::strip".to_string()][..]),
            "{res:?}"
        );
        assert_eq!(res.did_you_mean, ["b/svc.ts::Other", "b/svc.ts::strip"]);
        let json = serde_json::to_value(&res).unwrap();
        assert!(json["did_you_mean_by_target"].is_object(), "{json}");
    }

    #[test]
    fn a_qualifier_naming_nothing_is_not_found_with_suggestions() {
        let (dir, store) = fixture_files(&[
            (
                "a/svc.ts",
                "export class Svc { run() { return 1; } }\n\
                 export function strip(a: string) { return a; }\n",
            ),
            (
                "b/svc.ts",
                "export class Svc { run() { return 2; } }\n\
                 export class Other { run() { return 3; } }\n\
                 export function strip(b: string) { return b; }\n",
            ),
        ]);
        let reader = FilesystemReader::new(dir.path());
        let read = |spec: &str| read_symbols(&store, &[spec.to_string()], &reader, 0, None);

        let res = read("a/svc.ts::Svc::run");
        assert_eq!(res.symbols.len(), 1, "{res:?}");
        assert_eq!(res.symbols[0].path, "a/svc.ts");
        assert!(res.qualifier_ignored.is_empty(), "{res:?}");
        assert!(
            res.did_you_mean.is_empty(),
            "a hit suggests nothing: {res:?}"
        );

        let res = read("nothere.ts::missing");
        assert!(res.qualifier_ignored.is_empty(), "a miss is not a fallback");
        assert_eq!(res.not_found, ["nothere.ts::missing"]);

        // A wrong path: the bare name is unique, but it is not in that file.
        let res = read("nothere.ts::Other");
        assert!(res.symbols.is_empty(), "never widened to `Other`: {res:?}");
        assert!(res.qualifier_ignored.is_empty(), "{res:?}");
        assert_eq!(res.not_found, ["nothere.ts::Other"]);
        assert_eq!(res.did_you_mean, ["b/svc.ts::Other"], "{res:?}");

        // A wrong path whose bare name is ambiguous: not found, not ambiguous.
        let res = read("nothere.ts::run");
        assert!(res.ambiguous.is_empty(), "{res:?}");
        assert_eq!(res.not_found, ["nothere.ts::run"]);
        assert!(
            res.did_you_mean.iter().any(|s| s == "a/svc.ts::Svc::run"),
            "{res:?}"
        );

        // A wrong class.
        for spec in ["Nope::run", "a/svc.ts::Nope::run"] {
            let res = read(spec);
            assert!(
                res.symbols.is_empty() && res.ambiguous.is_empty(),
                "{spec}: {res:?}"
            );
            assert_eq!(res.not_found, [spec], "{res:?}");
            assert!(
                res.did_you_mean.iter().any(|s| s == "a/svc.ts::Svc::run"),
                "{spec}: {res:?}"
            );
        }
        assert_eq!(
            serde_json::to_value(read("nothere.ts::Other")).unwrap()["did_you_mean"],
            serde_json::json!(["b/svc.ts::Other"]),
            "suggestions are on the wire"
        );
        assert!(
            serde_json::to_value(read("a/svc.ts::Svc::run"))
                .unwrap()
                .get("did_you_mean")
                .is_none(),
            "absent when there are none"
        );

        let res = read("b/svc.ts::run");
        let [ambiguous] = res.ambiguous.as_slice() else {
            panic!("{res:?}")
        };
        assert!(!ambiguous.hint.contains("--repo"), "{ambiguous:?}");
        assert!(
            !ambiguous.hint.contains("pass a UID or path::name"),
            "{ambiguous:?}"
        );

        let res = read("run");
        let [ambiguous] = res.ambiguous.as_slice() else {
            panic!("{res:?}")
        };
        assert_eq!(ambiguous.hint, "pass a UID or path::Class::name");

        let res = read("strip");
        let [ambiguous] = res.ambiguous.as_slice() else {
            panic!("{res:?}")
        };
        assert_eq!(ambiguous.hint, "pass a UID or path::name");
    }

    /// nw-113 final review Minor 4: the hint never offers a `path::name`
    /// that would still be ambiguous, offers `path::Class::name` when a class
    /// tells same-file candidates apart, and `zzz::Svc::run` reads as
    /// `Svc::run` (disclosed in `qualifier_ignored`) because that reading
    /// still names something; it never falls back to `run`.
    #[test]
    fn ambiguity_hints_offer_only_what_disambiguates() {
        let (dir, store) = fixture_files(&[
            (
                "a/svc.ts",
                "export class Svc { run() { return 1; } }\n\
                 export class Other { run() { return 3; } }\n",
            ),
            ("b/svc.ts", "export class Svc { run() { return 2; } }\n"),
            (
                "c.ts",
                "export class Job { go() { return 1; } }\nexport function go() { return 2; }\n",
            ),
            (
                "d.py",
                "def helper():\n    return 1\n\n\ndef helper():\n    return 2\n",
            ),
            ("e.py", "def helper():\n    return 3\n"),
        ]);
        let reader = FilesystemReader::new(dir.path());
        let read = |spec: &str| read_symbols(&store, &[spec.to_string()], &reader, 0, None);

        let res = read("c.ts::go");
        let [ambiguous] = res.ambiguous.as_slice() else {
            panic!("{res:?}")
        };
        assert_eq!(ambiguous.hint, "pass a UID or path::Class::name", "{res:?}");

        let res = read("helper");
        let [ambiguous] = res.ambiguous.as_slice() else {
            panic!("{res:?}")
        };
        assert_eq!(ambiguous.candidates.len(), 3, "{res:?}");
        assert_eq!(
            ambiguous.hint, "pass a UID",
            "d.py::helper is still two: {res:?}"
        );

        let res = read("zzz::Svc::run");
        let [ambiguous] = res.ambiguous.as_slice() else {
            panic!("{res:?}")
        };
        let mut paths: Vec<&str> = ambiguous
            .candidates
            .iter()
            .map(|c| c.file_path.as_str())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            ["a/svc.ts", "b/svc.ts"],
            "Other::run is not Svc::run"
        );
        let [ignored] = res.qualifier_ignored.as_slice() else {
            panic!("{res:?}")
        };
        assert!(ignored.note.contains("'zzz'"), "{ignored:?}");
        assert!(ignored.note.contains("'Svc::run'"), "{ignored:?}");
    }

    /// I5: `nothere.ts::Svc::run` used to answer with another file's
    /// `Svc::run`. A qualifier that could be a file (a separator, an
    /// extension, or a real file's stem) is never dropped: the spec is not
    /// found and the shorter reading is the first suggestion. A bare word
    /// that names no file is still dropped, so module paths keep working.
    #[test]
    fn a_file_looking_qualifier_that_names_nothing_is_not_found() {
        let (dir, store) = fixture_files(&[
            ("a/svc.ts", "export class Svc { run() { return 1; } }\n"),
            ("c.ts", "export function go() { return 2; }\n"),
        ]);
        let reader = FilesystemReader::new(dir.path());
        let read = |spec: &str| read_symbols(&store, &[spec.to_string()], &reader, 0, None);

        for spec in [
            "nothere.ts::Svc::run",
            "lib/nothere::Svc::run",
            "c::Svc::run",
        ] {
            let res = read(spec);
            assert!(res.symbols.is_empty(), "{spec}: {res:?}");
            assert_eq!(res.not_found, [spec], "{res:?}");
            assert!(res.qualifier_ignored.is_empty(), "{spec}: {res:?}");
            assert_eq!(
                res.did_you_mean.first().map(String::as_str),
                Some("Svc::run"),
                "{spec}: {res:?}"
            );
        }

        // A bare word that names no file (a module-path segment) is dropped,
        // as before.
        for spec in ["zzz::Svc::run", "net::Svc::run"] {
            let res = read(spec);
            assert_eq!(res.symbols.len(), 1, "{spec}: {res:?}");
            assert_eq!(res.qualifier_ignored.len(), 1, "{spec}: {res:?}");
        }
    }

    /// nw-113 review: the owner is never a neighbour window; reading the
    /// first real symbol with neighbours does not drag in `<module>`.
    #[test]
    fn neighbour_expansion_skips_the_module_owner() {
        let (dir, store) = fixture_file(
            "src/app.ts",
            "export function f() {\n  return 1;\n}\nexport function g() {\n  return 2;\n}\nf();\n",
        );
        let reader = FilesystemReader::new(dir.path());
        let res = read_symbols(&store, &["f".to_string()], &reader, 1, None);
        let names: Vec<&str> = res.symbols.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["f", "g"], "{res:?}");
    }

    /// nw-113 review: the owner always spans the whole file, so a changed line
    /// count is not a relocation; only `end_line` follows the file.
    #[test]
    fn an_owner_read_never_reports_a_relocation() {
        let (dir, store) = fixture_file(
            "src/app.ts",
            "export function f() {\n  return 1;\n}\nf();\n",
        );
        let owner = store
            .symbols_in_file("src/app.ts")
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.is_module_owner())
            .expect("fixture mints an owner");
        assert_eq!(owner.end_line, 4);
        std::fs::write(
            dir.path().join("src/app.ts"),
            "export function f() {\n  return 1;\n}\nf();\nf();\n",
        )
        .unwrap();
        let w = read_one(&dir, &store, &owner.uid);
        assert_eq!(w.relocated_from_line, None, "{w:?}");
        assert_eq!((w.start_line, w.end_line), (1, 5), "{w:?}");
        assert!(!w.stale_span && w.body_available, "{w:?}");
    }

    /// nw-804: one symbol larger than `token_budget` is truncated to fit,
    /// with a marker line in the body and `body_truncated` in the JSON, for
    /// an ordinary function and a `<module>` owner alike. Its first lines are
    /// kept, and a budget the cut fits within is not reported as overrun.
    #[test]
    fn a_symbol_larger_than_the_budget_is_truncated_with_a_marker() {
        let body: String = (0..400)
            .map(|i| format!("  total += step({i});\n"))
            .collect();
        let calls: String = (0..400).map(|i| format!("step({i});\n")).collect();
        let (dir, store) = fixture_file(
            "src/big.js",
            &format!(
                "function step(n) {{ return n; }}\nfunction big(total) {{\n{body}  return total;\n}}\n{calls}"
            ),
        );
        let owner = store
            .symbols_in_file("src/big.js")
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.is_module_owner())
            .expect("fixture mints an owner");
        let reader = FilesystemReader::new(dir.path());
        for (spec, first_line) in [
            ("big", "function big(total) {"),
            (owner.uid.as_str(), "step(0);"),
        ] {
            let res = read_symbols(&store, &[spec.to_string()], &reader, 0, Some(200));
            assert_eq!(res.symbols.len(), 1, "{spec}: {res:?}");
            let w = &res.symbols[0];
            assert!(w.body_truncated, "{spec}: {w:?}");
            assert!(res.truncated, "{spec}");
            assert!(w.body.starts_with(first_line), "{spec}: {}", w.body);
            let marker = w.body.lines().last().unwrap();
            assert!(
                marker.starts_with(ELISION) && marker.contains("truncated"),
                "{spec}: {marker}"
            );
            assert!(window_cost(&w.body) <= 200, "{spec}: {}", w.body.len());
            assert!(
                res.budget_exceeded_by_first_symbol.is_none(),
                "{spec}: the truncated window fits the budget"
            );
            let json = serde_json::to_value(&res).unwrap();
            assert_eq!(json["symbols"][0]["body_truncated"], true, "{json}");
        }
        let res = read_symbols(&store, &["big".to_string()], &reader, 0, None);
        assert!(!res.symbols[0].body_truncated, "no budget, no cut");
    }

    /// nw-113: reading `<module>` returns the file's top-level code: the lines
    /// outside every function, class and method, with an elision marker
    /// between runs, not the whole file and not an empty stale window.
    #[test]
    fn reading_the_module_owner_returns_only_top_level_code() {
        let (dir, store) = fixture_file(
            "src/app.ts",
            "import { flushBuffer } from './lib';\nexport function f() {\n  return 1;\n}\n\nflushBuffer();\nf();\n",
        );
        let owner = store
            .symbols_in_file("src/app.ts")
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.is_module_owner())
            .expect("fixture mints an owner");
        let w = read_one(&dir, &store, &owner.uid);
        assert!(w.body_available && !w.stale_span, "{w:?}");
        assert_eq!(
            w.body,
            format!("import {{ flushBuffer }} from './lib';\n{ELISION}\nflushBuffer();\nf();")
        );
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
    fn unchanged_objective_c_declarations_read_their_own_spans() {
        let src = "@protocol MiniProtocol\n- (void)ping;\n@end\n@interface MiniInterface\n- (void)ping;\n@end\n@implementation MiniInterface\n- (void)ping {}\n@end\n";
        for path in ["src/mini.m", "src/mini.mm"] {
            let (dir, store) = fixture_file(path, src);
            for (name, kind, start, end) in [
                (
                    "MiniProtocol",
                    nestweaver_schema::SymbolKind::Interface,
                    1,
                    3,
                ),
                (
                    "MiniInterface",
                    nestweaver_schema::SymbolKind::Interface,
                    4,
                    6,
                ),
                ("MiniInterface", nestweaver_schema::SymbolKind::Class, 7, 9),
            ] {
                let symbols = store.lookup_symbols_by_name(name).unwrap();
                let sym = symbols.iter().find(|sym| sym.kind == kind).unwrap();
                let w = read_one(&dir, &store, &sym.uid);
                assert!(w.body_available && !w.stale_span, "{path}: {w:?}");
                assert_eq!((w.start_line, w.end_line), (start, end), "{w:?}");
                assert_eq!(w.body, read_span(src, start, end).unwrap(), "{w:?}");
                assert!(w.relocated_from_line.is_none(), "{w:?}");
            }
        }
    }

    #[test]
    fn renamed_objective_c_declarations_do_not_read_another_body() {
        let src = "@protocol MiniProtocol\n- (void)ping;\n@end\n@interface MiniInterface\n- (void)ping;\n@end\n@implementation MiniInterface\n- (void)ping {}\n@end\n";
        let (dir, store) = fixture_file("src/mini.m", src);
        fs::write(
            dir.path().join("src/mini.m"),
            src.replace("MiniProtocol", "OtherProtocol")
                .replace("MiniInterface", "OtherInterface"),
        )
        .unwrap();
        for name in ["MiniProtocol", "MiniInterface"] {
            for sym in store.lookup_symbols_by_name(name).unwrap() {
                let w = read_one(&dir, &store, &sym.uid);
                assert!(
                    w.stale_span && !w.body_available && w.body.is_empty(),
                    "{w:?}"
                );
            }
        }
    }

    #[test]
    fn objective_c_header_detection_preserves_annotation_and_name_guards() {
        for keyword in ["@protocol", "@interface", "@implementation"] {
            let body = format!("// comment\n\n{keyword} Target\n@end");
            assert!(span_holds_symbol(&body, "Target", "src/mini.m", ""));
            assert!(!span_holds_symbol(&body, "Other", "src/mini.m", ""));
            let annotation = format!(
                "{keyword}Like(\n Target,\n arg,\n arg,\n arg,\n arg\n)\nvoid other() {{}}"
            );
            assert!(!span_holds_symbol(&annotation, "Target", "src/mini.m", ""));
            // Even an exact Objective-C keyword remains a decorator in a
            // different language; a name in its arguments is not a declaration.
            let decorator = annotation.replace(&format!("{keyword}Like("), &format!("{keyword} ("));
            assert!(!span_holds_symbol(&decorator, "Target", "src/mini.py", ""));
        }
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
