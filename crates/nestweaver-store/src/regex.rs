//! Trigram-accelerated first-party regex search over indexed text.
//!
//! NestWeaver already stores searchable text in the graph: `Section.text_content`
//! (markdown brain bodies), `Note.title`, and `Symbol.signature`. This module
//! lets agents run a real `regex` against that text without shelling out to
//! `rg`/`grep`, with an optional trigram pre-filter to skip non-matching nodes.
//!
//! ## Correctness vs. optimization
//!
//! The trigram posting table is purely an optimization. Correctness never
//! depends on it: when no posting table exists (the `index --with-trigrams`
//! flag was not used) or when the pattern yields no usable literal trigrams
//! (e.g. `.{4,}`), we fall back to scanning every candidate node's text and
//! running the compiled regex against it. The trigram pre-filter only ever
//! *narrows* the candidate set — we always confirm with the real regex.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use lbug::Value;
use regex_syntax::hir::literal::Extractor;
use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::StoreError;

/// Hard cap on candidate nodes considered in a single search. When the
/// candidate set (after pre-filtering) exceeds this, results are truncated
/// and `truncated` is set on the response.
pub const CANDIDATE_CAP: usize = 5000;

/// Default wall-clock budget for a single search, in milliseconds.
pub const DEFAULT_MAX_MILLIS: u64 = 2000;

/// Maximum accepted regex pattern length, in bytes. A longer pattern is rejected
/// before compilation so an untrusted client cannot force a large compile just
/// by sending a huge pattern.
pub const MAX_PATTERN_BYTES: usize = 4096;

/// Compiled-program size limit for a single regex. The `regex` crate defaults to
/// 10 MiB; we cap lower because patterns arrive from untrusted clients. The
/// engine is finite-automata / linear-time (no catastrophic backtracking), so
/// this bounds compile CPU/memory, not match time.
const REGEX_SIZE_LIMIT: usize = 1 << 20; // 1 MiB

/// Compile an (untrusted) regex pattern with a length guard and a bounded
/// compiled-program size, so a pathological pattern returns a clean error
/// instead of consuming up to the crate's 10 MiB default.
fn compile_pattern(pattern: &str) -> Result<regex::Regex, StoreError> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(StoreError::Query(format!(
            "regex pattern too long: {} bytes (max {MAX_PATTERN_BYTES})",
            pattern.len()
        )));
    }
    regex::RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| StoreError::Query(format!("invalid regex: {e}")))
}

/// A single regex match hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegexMatch {
    /// Node UID (sym:..., sec:..., note:...).
    pub uid: String,
    /// Node kind discriminator: "Symbol", "Section", or "Note".
    pub kind: String,
    /// Human-readable title (symbol name, heading text, or note title).
    pub title: String,
    /// Location string (file path, optionally with line).
    pub location: String,
    /// 1-based line within the node's text where the first match occurred.
    pub line: Option<u32>,
    /// A short excerpt of the matched line.
    pub snippet: String,
}

/// Result of a `regex_search` call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegexSearchResult {
    pub results: Vec<RegexMatch>,
    /// True when the candidate cap or time budget was hit and results are partial.
    pub truncated: bool,
    /// True when the trigram pre-filter could not be used and we scanned all
    /// candidate text directly (either no posting table, or no usable literals).
    pub scanned_fallback: bool,
}

/// Per-file aggregate count for `count_patterns`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileCount {
    pub path: String,
    pub count: u64,
}

/// Per-pattern aggregate result for `count_patterns`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PatternCount {
    pub pattern: String,
    pub total_matches: u64,
    pub files_matched: u64,
    pub top_files: Vec<FileCount>,
}

/// A unit of indexed text plus its node metadata. Internal to this module.
struct Candidate {
    uid: String,
    kind: String,
    title: String,
    location: String,
    text: String,
    /// 1-based line in the source file where this candidate's `text` begins.
    /// Used to translate a match's line *within* `text` into a file line.
    /// `1` when the node has no meaningful file offset (e.g. Note titles).
    start_line: u32,
}

/// Lowercase 3-grams of `s`, deduplicated. Operates on Unicode scalar values
/// (chars), so multi-byte text is handled without panicking on byte slices.
fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.to_lowercase().chars().collect();
    let mut out = HashSet::new();
    if chars.len() < 3 {
        return out;
    }
    for w in chars.windows(3) {
        out.insert(w.iter().collect::<String>());
    }
    out
}

/// Extract the set of trigrams that any matching text MUST contain, expressed
/// as an AND-of-ORs (CNF): the outer Vec is ANDed, each inner set is ORed.
///
/// Returns `None` when the regex has no usable required literals (e.g. `.{4,}`,
/// leading `.*`, alternations with an empty branch) — the caller then falls
/// back to a full scan.
fn required_trigram_clauses(pattern: &str) -> Option<Vec<HashSet<String>>> {
    let hir = regex_syntax::parse(pattern).ok()?;
    let extractor = Extractor::new();
    // We want the set of literals that any match is *prefixed/seeded* by; the
    // "required" (suffix-anchored) seq is the conservative choice for a
    // pre-filter — every match must contain one of these literals.
    let seq = extractor.extract(&hir);

    // If the seq is infinite/inexact-without-finite-literals, we cannot use it.
    let literals = seq.literals()?;
    if literals.is_empty() {
        return None;
    }

    // Each literal becomes an OR-clause of its trigrams. A literal shorter than
    // 3 chars yields no trigrams → it cannot constrain the search, so the whole
    // prefilter is unusable (any text could match that branch).
    let mut clauses: Vec<HashSet<String>> = Vec::new();
    for lit in literals {
        // Inexact literals are prefixes/fragments; their trigrams are still a
        // necessary condition for the branch, so they remain usable.
        let lit_str = String::from_utf8_lossy(lit.as_bytes()).to_string();
        let tg = trigrams(&lit_str);
        if tg.is_empty() {
            // This alternation branch has no usable trigram → cannot prefilter.
            return None;
        }
        clauses.push(tg);
    }
    if clauses.is_empty() {
        return None;
    }
    Some(clauses)
}

impl GraphStore {
    /// True when the trigram posting table exists and has at least one row.
    /// Used to decide whether to attempt the pre-filter or go straight to a
    /// full scan.
    fn has_trigram_index(&self) -> bool {
        let conn = match self.conn() {
            Ok(c) => c,
            Err(_) => return false,
        };
        match conn.query("MATCH (t:TrigramPosting) RETURN t.trigram LIMIT 1") {
            Ok(result) => result.count() > 0,
            Err(_) => false,
        }
    }

    /// Build (or rebuild) the trigram posting table over all indexed text:
    /// `Section.text_content`, `Note.title`, and `Symbol.signature`.
    ///
    /// Idempotent: clears any existing postings first. Opt-in — only called by
    /// `index --with-trigrams`. Returns the number of (trigram, uid) postings
    /// written.
    pub fn build_trigram_index(&self) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        // Clear existing postings so a rebuild reflects the current graph.
        conn.query("MATCH (t:TrigramPosting) DETACH DELETE t")
            .map_err(|e| StoreError::Query(format!("clear trigram postings: {e}")))?;

        let candidates = self.collect_candidates(None, None)?;

        // Accumulate distinct (trigram, uid) pairs. A node contributes each of
        // its distinct trigrams once.
        let mut postings: Vec<(String, String)> = Vec::new();
        for c in &candidates {
            for tg in trigrams(&c.text) {
                postings.push((tg, c.uid.clone()));
            }
        }

        let mut stmt = conn
            .prepare("CREATE (:TrigramPosting {uid: $puid, trigram: $tg, node_uid: $nuid})")
            .map_err(|e| StoreError::Query(format!("prepare trigram insert: {e}")))?;
        for (i, (tg, nuid)) in postings.iter().enumerate() {
            // Synthetic primary key: index-stamped to stay unique.
            let puid = format!("tg:{i}");
            conn.execute(
                &mut stmt,
                vec![
                    ("puid", Value::String(puid)),
                    ("tg", Value::String(tg.clone())),
                    ("nuid", Value::String(nuid.clone())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("insert trigram: {e}")))?;
        }
        Ok(postings.len())
    }

    /// Collect all searchable candidate nodes (Sections, Notes, Symbols),
    /// optionally filtered by `path_prefix` (matched against location) and
    /// `kinds` (case-insensitive kind names: "Section", "Note", "Symbol").
    fn collect_candidates(
        &self,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
    ) -> Result<Vec<Candidate>, StoreError> {
        let want_kind = |k: &str| -> bool {
            match kinds {
                None => true,
                Some(ks) => ks.iter().any(|want| want.eq_ignore_ascii_case(k)),
            }
        };

        let mut out = Vec::new();

        // Sections — body text is the richest source. We need the parent note's
        // file_path for the location, so build a note_uid -> path map once.
        if want_kind("Section") {
            let notes = self.list_notes(None)?;
            let note_path: HashMap<String, String> =
                notes.into_iter().map(|n| (n.uid, n.file_path)).collect();
            for s in self.list_all_sections()? {
                if s.text_content.is_empty() {
                    continue;
                }
                let path = note_path.get(&s.note_uid).cloned().unwrap_or_default();
                let location = format!("{path}:{}", s.start_line);
                if let Some(prefix) = path_prefix
                    && !path.starts_with(prefix)
                {
                    continue;
                }
                out.push(Candidate {
                    uid: s.uid,
                    kind: "Section".to_string(),
                    title: String::new(),
                    location,
                    text: s.text_content,
                    start_line: s.start_line,
                });
            }
        }

        // Notes — index the title (the section body carries the rest).
        if want_kind("Note") {
            for n in self.list_notes(None)? {
                if n.title.is_empty() {
                    continue;
                }
                if let Some(prefix) = path_prefix
                    && !n.file_path.starts_with(prefix)
                {
                    continue;
                }
                out.push(Candidate {
                    uid: n.uid,
                    kind: "Note".to_string(),
                    title: n.title.clone(),
                    location: n.file_path,
                    text: n.title,
                    // A note's title has no meaningful file line offset.
                    start_line: 1,
                });
            }
        }

        // Symbols — signature text.
        if want_kind("Symbol") {
            for sym in self.list_all_symbols()? {
                if sym.signature.is_empty() {
                    continue;
                }
                if let Some(prefix) = path_prefix
                    && !sym.file_path.starts_with(prefix)
                {
                    continue;
                }
                let location = format!("{}:{}", sym.file_path, sym.start_line);
                out.push(Candidate {
                    uid: sym.uid,
                    kind: "Symbol".to_string(),
                    title: sym.name,
                    location,
                    text: sym.signature,
                    start_line: sym.start_line,
                });
            }
        }

        Ok(out)
    }

    /// Look up the candidate node UIDs that satisfy the trigram CNF clauses.
    /// For each AND-clause we union the postings of its OR-trigrams; the final
    /// candidate set is the intersection across all AND-clauses.
    ///
    /// Returns `None` if the posting table is empty (caller falls back to a
    /// full scan).
    fn trigram_candidate_uids(
        &self,
        clauses: &[HashSet<String>],
    ) -> Result<Option<HashSet<String>>, StoreError> {
        if !self.has_trigram_index() {
            return Ok(None);
        }
        let conn = self.conn()?;
        let mut acc: Option<HashSet<String>> = None;
        for clause in clauses {
            let mut clause_uids: HashSet<String> = HashSet::new();
            for tg in clause {
                let mut stmt = conn
                    .prepare("MATCH (t:TrigramPosting {trigram: $tg}) RETURN t.node_uid")
                    .map_err(|e| StoreError::Query(format!("prepare trigram lookup: {e}")))?;
                let result = conn
                    .execute(&mut stmt, vec![("tg", Value::String(tg.clone()))])
                    .map_err(|e| StoreError::Query(format!("trigram lookup: {e}")))?;
                for row in result {
                    if let Some(Value::String(uid)) = row.first() {
                        clause_uids.insert(uid.clone());
                    }
                }
            }
            acc = Some(match acc {
                None => clause_uids,
                Some(prev) => prev.intersection(&clause_uids).cloned().collect(),
            });
            // Early out: an empty intersection can never grow.
            if acc.as_ref().is_some_and(|s| s.is_empty()) {
                break;
            }
        }
        Ok(acc)
    }

    /// First-party regex search over indexed text with an optional trigram
    /// pre-filter. Always confirms candidate matches with the compiled regex,
    /// so results are correct regardless of whether the trigram index exists.
    pub fn regex_search(
        &self,
        pattern: &str,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
        limit: Option<usize>,
        max_millis: Option<u64>,
    ) -> Result<RegexSearchResult, StoreError> {
        let re = compile_pattern(pattern)?;
        let deadline_ms = max_millis.unwrap_or(DEFAULT_MAX_MILLIS);
        let start = Instant::now();
        let limit = limit.unwrap_or(usize::MAX);

        // Decide whether a trigram pre-filter is possible.
        let clauses = required_trigram_clauses(pattern);
        let prefilter_uids = match &clauses {
            Some(c) => self.trigram_candidate_uids(c)?,
            None => None,
        };
        // scanned_fallback: we did NOT narrow via trigrams (no literals, or no
        // posting table built).
        let scanned_fallback = prefilter_uids.is_none();

        let mut candidates = self.collect_candidates(path_prefix, kinds)?;
        if let Some(ref uids) = prefilter_uids {
            candidates.retain(|c| uids.contains(&c.uid));
        }

        let mut truncated = candidates.len() > CANDIDATE_CAP;
        if truncated {
            candidates.truncate(CANDIDATE_CAP);
        }

        let mut results = Vec::new();
        for c in &candidates {
            if start.elapsed().as_millis() as u64 > deadline_ms {
                truncated = true;
                break;
            }
            if let Some(m) = re.find(&c.text) {
                let (line_in_text, snippet) = line_and_snippet(&c.text, m.start());
                // Translate the line *within* the node's text into a file line:
                // the node's text starts at `c.start_line`, so the match on the
                // first text line (line_in_text == 1) is at c.start_line.
                let file_line = c.start_line.saturating_add(line_in_text.saturating_sub(1));
                results.push(RegexMatch {
                    uid: c.uid.clone(),
                    kind: c.kind.clone(),
                    title: c.title.clone(),
                    location: c.location.clone(),
                    line: Some(file_line),
                    snippet,
                });
                if results.len() >= limit {
                    truncated = truncated || candidates.len() > results.len();
                    break;
                }
            }
        }

        Ok(RegexSearchResult {
            results,
            truncated,
            scanned_fallback,
        })
    }

    /// Counts-only companion to `regex_search`. For each pattern, returns total
    /// match count (one per matching node), the number of distinct files that
    /// matched, and the top files by match count. Reuses the same trigram
    /// pre-filter and full-scan fallback.
    pub fn count_patterns(
        &self,
        patterns: &[String],
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
    ) -> Result<Vec<PatternCount>, StoreError> {
        // Collect candidates once and reuse across patterns.
        let all_candidates = self.collect_candidates(path_prefix, kinds)?;

        let mut out = Vec::new();
        for pattern in patterns {
            let re = compile_pattern(pattern)?;

            // Optional trigram narrowing.
            let clauses = required_trigram_clauses(pattern);
            let prefilter_uids = match &clauses {
                Some(c) => self.trigram_candidate_uids(c)?,
                None => None,
            };

            let mut per_file: HashMap<String, u64> = HashMap::new();
            let mut total: u64 = 0;
            for c in &all_candidates {
                if let Some(ref uids) = prefilter_uids
                    && !uids.contains(&c.uid)
                {
                    continue;
                }
                if re.is_match(&c.text) {
                    total += 1;
                    let file = file_of(&c.location);
                    *per_file.entry(file).or_insert(0) += 1;
                }
            }

            let files_matched = per_file.len() as u64;
            let mut top_files: Vec<FileCount> = per_file
                .into_iter()
                .map(|(path, count)| FileCount { path, count })
                .collect();
            top_files.sort_by(|a, b| b.count.cmp(&a.count).then(a.path.cmp(&b.path)));
            top_files.truncate(10);

            out.push(PatternCount {
                pattern: pattern.clone(),
                total_matches: total,
                files_matched,
                top_files,
            });
        }
        Ok(out)
    }
}

/// Strip a trailing `:<line>` suffix from a location to recover the file path.
fn file_of(location: &str) -> String {
    match location.rfind(':') {
        Some(idx) if location[idx + 1..].chars().all(|c| c.is_ascii_digit()) => {
            location[..idx].to_string()
        }
        _ => location.to_string(),
    }
}

/// Given the byte offset of a match within `text`, return its 1-based line
/// number and a trimmed snippet of that line.
fn line_and_snippet(text: &str, match_start: usize) -> (u32, String) {
    let line_idx = text[..match_start].matches('\n').count();
    let line = text.lines().nth(line_idx).unwrap_or("").trim();
    let snippet: String = line.chars().take(200).collect();
    (line_idx as u32 + 1, snippet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{Note, NoteKind, Section, Symbol, SymbolKind, Visibility};

    fn store_with_text() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();

        // A note + section carrying body text.
        store
            .insert_note(&Note {
                uid: "note:v:1".to_string(),
                vault_uid: "vlt:v".to_string(),
                file_path: "notes/auth.md".to_string(),
                title: "Authentication".to_string(),
                note_kind: NoteKind::General,
                word_count: 0,
                content_hash: "h".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:v:1:a".to_string(),
                note_uid: "note:v:1".to_string(),
                heading_uid: None,
                start_line: 5,
                end_line: 9,
                text_hash: "th".to_string(),
                text_content: "The login flow calls authenticateUser before issuing a token."
                    .to_string(),
                word_count: 10,
                pagerank_score: None,
            })
            .unwrap();

        // A symbol carrying signature text.
        store
            .insert_symbol(&Symbol {
                uid: "sym:1".to_string(),
                name: "authenticateUser".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:1".to_string(),
                file_path: "src/auth.rs".to_string(),
                start_line: 42,
                end_line: 60,
                signature: "fn authenticateUser(req: Request) -> Result<Token>".to_string(),
                summary: None,
                content_hash: "c".to_string(),
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

        store
    }

    #[test]
    fn regex_search_finds_pattern_in_section_and_symbol_without_index() {
        let store = store_with_text();
        // No trigram index built → must fall back to a direct scan and still
        // find the matches.
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(
            res.scanned_fallback,
            "no index → scanned_fallback should be true"
        );
        let uids: HashSet<&str> = res.results.iter().map(|m| m.uid.as_str()).collect();
        assert!(uids.contains("sec:v:1:a"), "should match the section body");
        assert!(uids.contains("sym:1"), "should match the symbol signature");
    }

    #[test]
    fn regex_search_uses_trigram_index_when_built() {
        let store = store_with_text();
        let written = store.build_trigram_index().unwrap();
        assert!(written > 0, "expected trigram postings to be written");

        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(
            !res.scanned_fallback,
            "with index + literal pattern, prefilter should be used"
        );
        let uids: HashSet<&str> = res.results.iter().map(|m| m.uid.as_str()).collect();
        assert!(uids.contains("sec:v:1:a"));
        assert!(uids.contains("sym:1"));

        // Line/snippet metadata is populated. The reported line is the symbol's
        // real start_line in the file (42), not 1.
        let sym_hit = res.results.iter().find(|m| m.uid == "sym:1").unwrap();
        assert_eq!(sym_hit.line, Some(42));
        assert!(sym_hit.snippet.contains("authenticateUser"));
    }

    #[test]
    fn no_literal_pattern_falls_back_to_scan() {
        let store = store_with_text();
        store.build_trigram_index().unwrap();
        // `.{4,}` has no usable literals → must scan all candidates.
        let res = store.regex_search(".{4,}", None, None, None, None).unwrap();
        assert!(
            res.scanned_fallback,
            "no-literal pattern must set scanned_fallback even with an index present"
        );
        // It still matches every text-bearing node.
        assert!(!res.results.is_empty());
    }

    #[test]
    fn count_patterns_matches_manual_count() {
        let store = store_with_text();
        // "token" appears in the section body and the symbol signature (Token).
        // Case-sensitive "token" → only the section (lowercase) matches.
        let counts = store
            .count_patterns(&["token".to_string()], None, None)
            .unwrap();
        assert_eq!(counts.len(), 1);
        let c = &counts[0];
        assert_eq!(c.pattern, "token");
        // Manual count: section body contains "token"; symbol sig has "Token"
        // (capital) which does not match case-sensitively. Note title doesn't.
        assert_eq!(
            c.total_matches, 1,
            "only the section matches lowercase 'token'"
        );
        assert_eq!(c.files_matched, 1);
        assert_eq!(c.top_files[0].path, "notes/auth.md");

        // Case-insensitive pattern picks up both the section and the symbol.
        let counts_ci = store
            .count_patterns(&["(?i)token".to_string()], None, None)
            .unwrap();
        assert_eq!(counts_ci[0].total_matches, 2);
        assert_eq!(counts_ci[0].files_matched, 2);
    }

    #[test]
    fn symbol_match_reports_real_start_line_not_one() {
        // QA bug B: a Symbol's text is its signature, but the reported `line`
        // must be the symbol's real start_line in the file, not 1.
        let store = store_with_text();
        let res = store
            .regex_search(
                "authenticateUser",
                None,
                Some(&["Symbol".to_string()]),
                None,
                None,
            )
            .unwrap();
        let sym_hit = res
            .results
            .iter()
            .find(|m| m.uid == "sym:1")
            .expect("symbol match present");
        // The symbol is defined at line 42 (see store_with_text()).
        assert_eq!(
            sym_hit.line,
            Some(42),
            "symbol match must report real start_line, not 1"
        );
    }

    #[test]
    fn section_match_reports_line_offset_by_section_start() {
        // QA bug B: a Section body match reports the line *within the file*,
        // offset by the section's start_line (5). The match is on the section's
        // first body line, so the reported line should be 5.
        let store = store_with_text();
        let res = store
            .regex_search(
                "authenticateUser",
                None,
                Some(&["Section".to_string()]),
                None,
                None,
            )
            .unwrap();
        let sec_hit = res
            .results
            .iter()
            .find(|m| m.uid == "sec:v:1:a")
            .expect("section match present");
        assert_eq!(
            sec_hit.line,
            Some(5),
            "section match must be offset by section start_line"
        );
    }

    #[test]
    fn kinds_filter_restricts_candidates() {
        let store = store_with_text();
        let res = store
            .regex_search(
                "authenticateUser",
                None,
                Some(&["Symbol".to_string()]),
                None,
                None,
            )
            .unwrap();
        let uids: HashSet<&str> = res.results.iter().map(|m| m.uid.as_str()).collect();
        assert!(uids.contains("sym:1"));
        assert!(
            !uids.contains("sec:v:1:a"),
            "section excluded by kinds filter"
        );
    }

    #[test]
    fn compile_pattern_rejects_overlong_pattern() {
        let pattern = "a".repeat(MAX_PATTERN_BYTES + 1);
        let err = compile_pattern(&pattern).unwrap_err();
        assert!(
            format!("{err}").contains("too long"),
            "overlong pattern should be rejected before compile: {err}"
        );
    }

    #[test]
    fn compile_pattern_rejects_oversized_program() {
        // A bounded but enormous repetition compiles to a program far larger than
        // the 1 MiB size limit, so build() must error rather than allocate it.
        let err = compile_pattern("a{1000}{1000}").unwrap_err();
        assert!(
            format!("{err}").contains("invalid regex"),
            "oversized compiled program should be rejected: {err}"
        );
    }

    #[test]
    fn compile_pattern_accepts_normal_pattern() {
        assert!(compile_pattern(r"fn\s+\w+\(").is_ok());
    }
}
