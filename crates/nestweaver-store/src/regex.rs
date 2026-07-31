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
//! flag was not used), when the pattern yields no usable literal trigrams
//! (e.g. `.{4,}`), or when the posting table is *stale* (the graph changed
//! since it was built), we fall back to scanning every candidate
//! node's text and running the compiled regex against it. The trigram
//! pre-filter only ever *narrows* the candidate set — we always confirm with
//! the real regex.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use lbug::Value;
use regex_syntax::hir::literal::Extractor;
use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::StoreError;
use crate::tantivy_index::SEARCH_PRESENTATION_LIMIT_MAX;

/// Safety ceiling on how many candidate nodes a single search will SCAN before
/// stopping (and honestly reporting `truncated`). This is a last-resort bound
/// well above any realistic corpus — the search is normally bounded by the
/// wall-clock `deadline_ms`, not this. It must NOT be used to pre-truncate the
/// candidate list before scanning: doing so (as an earlier 5000 cap did) drops
/// real matches that sort past the cap in collect order — Sections → Notes →
/// Symbols, so symbol matches on a large graph were systematically missed and
/// dishonestly reported as `truncated:true, results:[]` (nw-076).
pub const CANDIDATE_CAP: usize = 200_000;

/// Default wall-clock budget for a single search, in milliseconds.
pub const DEFAULT_MAX_MILLIS: u64 = 2000;

/// Maximum accepted regex pattern length, in bytes. A longer pattern is rejected
/// before compilation so an untrusted client cannot force a large compile just
/// by sending a huge pattern.
pub const MAX_PATTERN_BYTES: usize = 4096;

/// `Meta` table key under which `build_trigram_index` records provenance for
/// the posting table as `"<graph_generation>:<candidate_node_count>"`. A
/// reader compares both values against the current graph; any drift means the
/// postings no longer reflect the indexed text and the pre-filter must not be
/// used.
const TRIGRAM_INDEX_META_KEY: &str = "trigram_index";

/// One-shot latch so the stale-index warning is printed once per process
/// instead of on every search.
static TRIGRAM_STALE_WARNED: AtomicBool = AtomicBool::new(false);

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
    /// True when a trigram posting table EXISTS but was bypassed because it is
    /// stale (the graph changed since it was built). This lets callers
    /// surface staleness in-band: on the daemon path the once-per-process
    /// stderr warning is invisible. Implies `scanned_fallback` when the
    /// pattern has usable literals; always false when no index was ever built.
    #[serde(default)]
    pub stale_index: bool,
    /// Human-readable explanation when an empty result is NOT a definitive
    /// "no matches exist".
    ///
    /// nw-097: the MCP tool attached this note itself, so a CLI caller saw
    /// `{"results": [], "truncated": true}` with nothing explaining it and
    /// reasonably read it as "the pattern matches nothing". `truncated` alone
    /// does not carry that meaning to a human. Living on the shared result
    /// means every surface — CLI, MCP, daemon — reports it identically rather
    /// than each remembering to bolt it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The note attached to an empty-but-truncated regex search.
///
/// A search that ran out of scan budget before matching anything has NOT
/// established that no matches exist, and must not be presented as if it had.
pub const SCAN_BUDGET_NOTE: &str = "Pattern matched no candidates within the scan budget. Results may exist beyond \
     the scanned range.";

impl RegexSearchResult {
    /// Attach [`SCAN_BUDGET_NOTE`] when this result is empty only because the
    /// scan was cut short. Idempotent, and never overwrites an existing note.
    pub fn with_scan_budget_note(mut self) -> Self {
        if self.results.is_empty() && self.truncated && self.note.is_none() {
            self.note = Some(SCAN_BUDGET_NOTE.to_string());
        }
        self
    }
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
    /// True when a trigram posting table EXISTS but was bypassed because it is
    /// stale (the graph changed since it was built), so this pattern
    /// was counted via a full scan. Lets callers surface staleness in-band on
    /// the daemon path, where the stderr warning is invisible.
    pub stale_index: bool,
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
///
/// The fold is per-char (`char::to_lowercase`), NOT `str::to_lowercase`: the
/// `str` version is context-sensitive — Greek 'Σ' folds to final 'ς' at a word
/// boundary but 'σ' mid-word. The index side folds whole note bodies while the
/// query side folds short extracted literals, so the same characters could
/// fold differently on each side and the pre-filter would silently drop real
/// matches (e.g. pattern `ΓΟΣ` vs. text `ΓΟΣΑΝΘΡΑΞ`). A context-free per-char
/// fold keeps the two sides consistent.
fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().flat_map(char::to_lowercase).collect();
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
/// The literal extractor already resolves alternations into alternative
/// literals: for `(alpha|beta)` it yields both `alpha` and `beta`, and a match
/// needs only ONE of them. All extracted literals are therefore unioned into
/// a single OR clause. ANDing them per literal (the earlier behavior)
/// required every alternation branch to appear in the same text and silently
/// dropped real matches.
///
/// Returns `None` when the regex has no usable required literals (e.g. `.{4,}`,
/// leading `.*`) or when ANY literal yields no trigrams (e.g. an alternation
/// branch shorter than 3 chars, or a non-literal branch): that branch is then
/// unconstrainable, so the only safe pre-filter is none at all — the caller
/// falls back to a full scan.
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

    // Union every literal's trigrams into ONE OR clause: the literals are
    // alternatives (a match needs any one of them), not conjuncts. A literal
    // shorter than 3 chars yields no trigrams → that branch cannot constrain
    // the search, so the whole prefilter is unusable.
    let mut clause: HashSet<String> = HashSet::new();
    for lit in literals {
        // Inexact literals are prefixes/fragments; their trigrams are still a
        // necessary condition for the branch, so they remain usable.
        let lit_str = String::from_utf8_lossy(lit.as_bytes()).to_string();
        let tg = trigrams(&lit_str);
        if tg.is_empty() {
            // This alternation branch has no usable trigram → cannot prefilter.
            return None;
        }
        clause.extend(tg);
    }
    if clause.is_empty() {
        return None;
    }
    Some(vec![clause])
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
        let setup_conn = self.conn()?;
        // Mark the index as mid-build BEFORE touching the postings. An
        // interrupted rebuild (crash/kill between the clear below and the
        // provenance write at the end) would otherwise leave the OLD
        // provenance in place — matching the current generation and looking
        // "fresh" while the posting table is empty or partial, silently
        // dropping regex matches. "building" is unparseable by
        // `trigram_index_meta`, so any interrupted build reads as stale and
        // falls back to a full scan. Written on its own auto-committed
        // connection so it survives the build transaction rolling back.
        Self::write_trigram_meta(&setup_conn, "building")?;

        let candidates = self.collect_candidates(None, None)?;

        // Accumulate distinct (trigram, uid) pairs. A node contributes each of
        // its distinct trigrams once.
        let mut postings: Vec<(String, String)> = Vec::new();
        for c in &candidates {
            for tg in trigrams(&c.text) {
                postings.push((tg, c.uid.clone()));
            }
        }

        // Clear + rebuild + provenance in ONE explicit transaction: per-
        // statement auto-commit made every posting its own WAL flush, which
        // is fsync-bound and impractically slow at monorepo scale (20+
        // minutes on a ~25 MB DB). A single transaction flushes once. It
        // also makes interruption all-or-nothing: a crashed build rolls
        // back, leaving the durable "building" marker above to report the
        // index as stale.
        let conn = self.begin_transaction()?;
        let build = (|| -> Result<usize, StoreError> {
            // Clear existing postings so a rebuild reflects the current graph.
            conn.query("MATCH (t:TrigramPosting) DETACH DELETE t")
                .map_err(|e| StoreError::Query(format!("clear trigram postings: {e}")))?;

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

            // Record provenance so a later reader can detect that the posting
            // table no longer reflects the graph (nodes added/edited after this
            // build) and fall back to a full scan instead of silently missing
            // matches. Overwrites the "building" marker written at the
            // start of this rebuild.
            let meta_value = format!("{}:{}", self.graph_generation(), candidates.len());
            Self::write_trigram_meta(&conn, &meta_value)?;
            Ok(postings.len())
        })();
        match build {
            Ok(n) => {
                self.commit_transaction(&conn)?;
                Ok(n)
            }
            Err(e) => {
                let _ = self.rollback_transaction(&conn);
                Err(e)
            }
        }
    }

    /// Upsert the trigram-index provenance singleton in the `Meta` table
    /// (same delete+insert pattern as the other `Meta` singletons).
    fn write_trigram_meta(conn: &lbug::Connection<'_>, value: &str) -> Result<(), StoreError> {
        let mut del = conn
            .prepare("MATCH (m:Meta {key: $k}) DETACH DELETE m")
            .map_err(|e| StoreError::Query(format!("prepare trigram meta delete: {e}")))?;
        conn.execute(
            &mut del,
            vec![("k", Value::String(TRIGRAM_INDEX_META_KEY.to_string()))],
        )
        .map_err(|e| StoreError::Query(format!("clear trigram meta: {e}")))?;
        let mut ins = conn
            .prepare("CREATE (:Meta {key: $k, value: $v})")
            .map_err(|e| StoreError::Query(format!("prepare trigram meta insert: {e}")))?;
        conn.execute(
            &mut ins,
            vec![
                ("k", Value::String(TRIGRAM_INDEX_META_KEY.to_string())),
                ("v", Value::String(value.to_string())),
            ],
        )
        .map_err(|e| StoreError::Query(format!("write trigram meta: {e}")))?;
        Ok(())
    }

    /// Read the provenance recorded by [`GraphStore::build_trigram_index`].
    /// Returns `None` when the `Meta` table or key is absent (postings built
    /// before provenance tracking) or the value is unparseable.
    fn trigram_index_meta(&self) -> Option<(u64, u64)> {
        let conn = self.conn().ok()?;
        let mut stmt = conn
            .prepare("MATCH (m:Meta {key: $k}) RETURN m.value")
            .ok()?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("k", Value::String(TRIGRAM_INDEX_META_KEY.to_string()))],
            )
            .ok()?;
        for row in result {
            if let Some(Value::String(value)) = row.first() {
                let (generation, count) = value.split_once(':')?;
                return Some((generation.parse().ok()?, count.parse().ok()?));
            }
        }
        None
    }

    /// Number of text-bearing candidate nodes (Sections with body text, Notes
    /// with a title, Symbols with a signature) — the same set
    /// `collect_candidates(None, None)` would yield, counted cheaply.
    fn searchable_candidate_count(&self) -> Result<u64, StoreError> {
        let conn = self.conn()?;
        let mut total = 0u64;
        for query in [
            "MATCH (s:Section) WHERE s.text_content <> '' RETURN count(s)",
            "MATCH (n:Note) WHERE n.title <> '' RETURN count(n)",
            "MATCH (s:Symbol) WHERE s.signature <> '' RETURN count(s)",
        ] {
            let result = conn
                .query(query)
                .map_err(|e| StoreError::Query(format!("candidate count: {e}")))?;
            for row in result {
                if let Some(Value::Int64(n)) = row.first() {
                    total += (*n).max(0) as u64;
                }
            }
        }
        Ok(total)
    }

    /// True when the posting table exists but was built against a different
    /// graph state: the graph generation advanced (any mutation, e.g. an
    /// in-place edit) or the candidate-node count drifted (nodes added or
    /// removed) since `index --with-trigrams` ran. Postings without
    /// provenance (built by an older version) cannot be trusted and count as
    /// stale.
    fn trigram_index_is_stale(&self) -> bool {
        let Some((indexed_generation, indexed_count)) = self.trigram_index_meta() else {
            return true;
        };
        if indexed_generation != self.graph_generation() {
            return true;
        }
        match self.searchable_candidate_count() {
            Ok(count) => count != indexed_count,
            // On a read error, distrust the index (safe direction).
            Err(_) => true,
        }
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
    /// Returns `(uids, stale_index)`. `uids` is `None` when the posting table
    /// is missing OR stale (the graph changed since it was built); the
    /// caller then falls back to a full scan. `stale_index` is true only when
    /// a posting table exists but was bypassed as stale, so callers can
    /// surface that in-band (the stderr warning below fires once per process
    /// and is invisible on the daemon path). Observing a fresh (non-stale)
    /// index re-arms the one-shot warning latch, so a long-lived daemon warns
    /// again if a later rebuild is followed by another restale.
    fn trigram_candidate_uids(
        &self,
        clauses: &[HashSet<String>],
    ) -> Result<(Option<HashSet<String>>, bool), StoreError> {
        if !self.has_trigram_index() {
            return Ok((None, false));
        }
        if self.trigram_index_is_stale() {
            // Correctness first: a stale pre-filter can drop real matches
            // (nodes added after the build are invisible to it), so fall back
            // to a full scan and say so once on stderr.
            if !TRIGRAM_STALE_WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "warning: trigram index is stale (graph changed since it was built); \
                     falling back to full scan — rerun `index --with-trigrams` to restore the pre-filter"
                );
            }
            return Ok((None, true));
        }
        // A fresh index was observed: re-arm the one-shot warning latch so a
        // long-lived process can warn again after a rebuild + restale.
        TRIGRAM_STALE_WARNED.store(false, Ordering::Relaxed);
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
        Ok((acc, false))
    }

    /// First-party regex search over indexed text with an optional trigram
    /// pre-filter. Always confirms candidate matches with the compiled regex,
    /// so results are correct regardless of whether the trigram index exists.
    ///
    /// `limit` is bounded by [`SEARCH_PRESENTATION_LIMIT_MAX`], the same
    /// presentation ceiling `brain_search` enforces, so both search entry
    /// points accept the same range.
    pub fn regex_search(
        &self,
        pattern: &str,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
        limit: Option<usize>,
        max_millis: Option<u64>,
    ) -> Result<RegexSearchResult, StoreError> {
        if let Some(l) = limit
            && l > SEARCH_PRESENTATION_LIMIT_MAX
        {
            return Err(StoreError::Query(format!(
                "regex_search limit {l} exceeds the presentation maximum of {SEARCH_PRESENTATION_LIMIT_MAX}"
            )));
        }
        let re = compile_pattern(pattern)?;
        let deadline_ms = max_millis.unwrap_or(DEFAULT_MAX_MILLIS);
        let start = Instant::now();
        let limit = limit.unwrap_or(usize::MAX);

        // Decide whether a trigram pre-filter is possible. A pattern with no
        // usable literals never consults the index, so it reports no staleness.
        let clauses = required_trigram_clauses(pattern);
        let (prefilter_uids, stale_index) = match &clauses {
            Some(c) => self.trigram_candidate_uids(c)?,
            None => (None, false),
        };
        // scanned_fallback: we did NOT narrow via trigrams (no literals, or no
        // posting table built).
        let scanned_fallback = prefilter_uids.is_none();

        let mut candidates = self.collect_candidates(path_prefix, kinds)?;
        if let Some(ref uids) = prefilter_uids {
            candidates.retain(|c| uids.contains(&c.uid));
        }

        // Scan the full candidate set, bounded by the wall-clock deadline (and a
        // high safety ceiling) — NOT a low pre-truncation. `truncated` is set
        // ONLY when the scan actually stops early, so `truncated:true` with an
        // empty `results` now genuinely means "incomplete scan" rather than
        // "the match was ordered past a 5000 cap and never scanned" (nw-076).
        let mut truncated = false;
        let mut results = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            if start.elapsed().as_millis() as u64 > deadline_ms || i >= CANDIDATE_CAP {
                truncated = true;
                break;
            }
            if let Some(m) = re.find(&c.text) {
                // Check the limit BEFORE pushing: with `--limit 0` the caller
                // asked for no results, so even the first match must not be
                // returned (previously one result slipped through).
                if results.len() >= limit {
                    truncated = true;
                    break;
                }
                let (line_in_text, snippet) = line_and_snippet(&c.text, m.start());
                // Translate the line *within* the node's text into a file line:
                // the node's text starts at `c.start_line`, so the match on the
                // first text line (line_in_text == 1) is at c.start_line.
                let file_line = c.start_line.saturating_add(line_in_text.saturating_sub(1));
                // Point the location at the match's file line, not the node's
                // start line, so plain-text renderings of `location` show where
                // the match actually is (G9). Note locations carry no `:line`
                // suffix and are left untouched.
                let location = match c.location.rfind(':') {
                    Some(idx)
                        if !c.location[idx + 1..].is_empty()
                            && c.location[idx + 1..].chars().all(|ch| ch.is_ascii_digit()) =>
                    {
                        format!("{}:{file_line}", &c.location[..idx])
                    }
                    _ => c.location.clone(),
                };
                results.push(RegexMatch {
                    uid: c.uid.clone(),
                    kind: c.kind.clone(),
                    title: c.title.clone(),
                    location,
                    line: Some(file_line),
                    snippet,
                });
                if results.len() >= limit {
                    truncated = truncated || candidates.len() > results.len();
                    break;
                }
            }
        }

        // nw-097: attach the note at the source so no caller has to remember.
        Ok(RegexSearchResult {
            results,
            truncated,
            scanned_fallback,
            stale_index,
            note: None,
        }
        .with_scan_budget_note())
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
            let (prefilter_uids, stale_index) = match &clauses {
                Some(c) => self.trigram_candidate_uids(c)?,
                None => (None, false),
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
                stale_index,
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
    /// nw-097: an empty result that is empty only because the scan budget ran
    /// out must SAY so. Previously the MCP tool attached this note itself, so a
    /// CLI `--json` caller received `{"results": [], "truncated": true}` with
    /// nothing explaining it and would reasonably read it as "no matches exist".
    /// Attaching it on the shared result is what makes every surface agree.
    #[test]
    fn empty_and_truncated_carries_the_scan_budget_note() {
        let r = RegexSearchResult {
            results: vec![],
            truncated: true,
            scanned_fallback: false,
            stale_index: false,
            note: None,
        }
        .with_scan_budget_note();
        assert_eq!(r.note.as_deref(), Some(SCAN_BUDGET_NOTE));
    }

    /// A genuinely exhaustive empty search HAS established that nothing matches,
    /// so it must not be hedged — the note would be a false caveat.
    #[test]
    fn empty_but_complete_search_gets_no_note() {
        let r = RegexSearchResult {
            results: vec![],
            truncated: false,
            scanned_fallback: false,
            stale_index: false,
            note: None,
        }
        .with_scan_budget_note();
        assert!(r.note.is_none(), "a complete empty scan must not be hedged");
    }

    /// Results present means the caller has something concrete; the budget note
    /// is about an EMPTY result being misread.
    #[test]
    fn truncated_with_results_gets_no_scan_budget_note() {
        let hit = RegexMatch {
            uid: "sym:x".into(),
            kind: "Symbol".into(),
            title: "x".into(),
            location: "a.rs".into(),
            line: Some(1),
            snippet: "x".into(),
        };
        let r = RegexSearchResult {
            results: vec![hit],
            truncated: true,
            scanned_fallback: false,
            stale_index: false,
            note: None,
        }
        .with_scan_budget_note();
        assert!(r.note.is_none());
    }

    use super::*;
    use nestweaver_schema::{Note, NoteKind, Section, Symbol, SymbolKind, Visibility};

    /// Serializes tests that touch the process-global TRIGRAM_STALE_WARNED
    /// latch: a parallel stale observation (which sets the latch) can land
    /// between the fresh-index re-arm and its assertion, flaking
    /// `fresh_index_observation_rearms_stale_warning_latch` under load (seen
    /// on CI). Every stale-observation test must hold this lock too.
    static LATCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[test]
    fn regex_search_does_not_drop_a_match_ordered_late_in_the_candidate_set() {
        // nw-076: the fallback scan used to pre-truncate the candidate list to
        // the first 5000 nodes in collect order (Sections → Notes → Symbols), so
        // a match on a symbol ordered past the cap was silently dropped and
        // dishonestly reported as `truncated:true, results:[]`. With the fix the
        // full set is scanned (bounded only by the deadline / high safety
        // ceiling), so a late-ordered match is always found.
        let store = GraphStore::in_memory().unwrap();
        // Many non-matching symbols first, then the sole match LAST.
        for i in 0..300 {
            store
                .insert_symbol(&Symbol {
                    uid: format!("sym:pad:{i}"),
                    name: format!("filler_{i}"),
                    kind: SymbolKind::Function,
                    repo_uid: "repo:1".to_string(),
                    file_path: "src/pad.rs".to_string(),
                    start_line: 1,
                    end_line: 1,
                    signature: format!("fn filler_{i}()"),
                    summary: None,
                    content_hash: format!("h{i}"),
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
        store
            .insert_symbol(&Symbol {
                uid: "sym:needle".to_string(),
                name: "zzz_uniquely_ordered_last".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:1".to_string(),
                file_path: "src/needle.rs".to_string(),
                start_line: 7,
                end_line: 7,
                signature: "fn zzz_uniquely_ordered_last()".to_string(),
                summary: None,
                content_hash: "hn".to_string(),
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

        let res = store
            .regex_search("zzz_uniquely_ordered_last", None, None, None, None)
            .unwrap();
        assert!(res.scanned_fallback, "no trigram index → scanned_fallback");
        let uids: HashSet<&str> = res.results.iter().map(|m| m.uid.as_str()).collect();
        assert!(
            uids.contains("sym:needle"),
            "the late-ordered match must be found, got {} results",
            res.results.len()
        );
        // Honesty invariant: a fully-scanned corpus never reports truncated with
        // an empty result set.
        assert!(
            !(res.truncated && res.results.is_empty()),
            "must never return truncated:true with empty results on a scannable corpus"
        );
        assert!(!res.truncated, "small corpus scans fully, so not truncated");
    }

    /// An alternation is an OR, so the trigram pre-filter must union the
    /// branch literals — ANDing them per branch used to drop every match
    /// unless ALL branches appeared in the same text. The pre-filtered result
    /// set must be identical to the full-scan result set across the matrix.
    #[test]
    fn alternation_prefilter_matches_full_scan() {
        let patterns = [
            "(login|token)",
            "(alpha|beta|gamma)",
            "(authenticateUser|needle_word)",
            "((login|issuing) flow|Result)",
        ];
        for pattern in patterns {
            let scan = store_with_text()
                .regex_search(pattern, None, None, None, None)
                .unwrap();
            assert!(scan.scanned_fallback, "{pattern}: baseline must scan");

            let indexed = store_with_text();
            indexed.build_trigram_index().unwrap();
            let filtered = indexed
                .regex_search(pattern, None, None, None, None)
                .unwrap();
            let scan_uids: HashSet<&str> = scan.results.iter().map(|m| m.uid.as_str()).collect();
            let filtered_uids: HashSet<&str> =
                filtered.results.iter().map(|m| m.uid.as_str()).collect();
            assert_eq!(
                scan_uids, filtered_uids,
                "{pattern}: trigram pre-filter must not change the result set"
            );
            if pattern == "(authenticateUser|needle_word)" {
                assert!(
                    !filtered.scanned_fallback,
                    "{pattern}: literal alternation must still use the pre-filter"
                );
                assert!(
                    filtered_uids.contains("sec:v:1:a") && filtered_uids.contains("sym:1"),
                    "{pattern}: matches from the literal branch must survive the pre-filter"
                );
            }
        }
    }

    /// When ANY alternation branch yields no usable trigram (too short
    /// or non-literal), the pre-filter must be disabled entirely — unioning
    /// only the literal branches would drop matches coming from the other
    /// branch.
    #[test]
    fn alternation_with_unusable_branch_disables_prefilter() {
        let store = store_with_text();
        store.build_trigram_index().unwrap();
        for pattern in ["(tokenize|ok)", "(authenticateUser|x.*)"] {
            let res = store.regex_search(pattern, None, None, None, None).unwrap();
            assert!(
                res.scanned_fallback,
                "{pattern}: a branch without usable trigrams must disable the pre-filter"
            );
        }
        // "ok" (the short branch) matches inside "token" in the section body;
        // a pre-filter built from "tokenize" alone would have missed it.
        let res = store
            .regex_search("(tokenize|ok)", None, None, None, None)
            .unwrap();
        assert!(
            res.results.iter().any(|m| m.uid == "sec:v:1:a"),
            "the unconstrained branch's match must be found via the scan"
        );
    }

    /// Nodes added after the trigram build must not be invisible — the
    /// stale posting table is detected and the search falls back to a full
    /// scan that still finds the new node.
    #[test]
    fn stale_trigram_index_falls_back_to_scan_and_finds_new_nodes() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        let store = store_with_text();
        store.build_trigram_index().unwrap();

        // Sanity: a fresh index is used (no drift since the build).
        let fresh = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(
            !fresh.scanned_fallback,
            "freshly built index must be trusted"
        );

        // Add a node AFTER the build. The raw insert does not touch the
        // generation counter, so this exercises the candidate-count staleness
        // signal.
        store
            .insert_symbol(&Symbol {
                uid: "sym:2".to_string(),
                name: "postbuildHook".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:1".to_string(),
                file_path: "src/hook.rs".to_string(),
                start_line: 3,
                end_line: 9,
                signature: "fn postbuildHook()".to_string(),
                summary: None,
                content_hash: "c2".to_string(),
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

        let res = store
            .regex_search("postbuildHook", None, None, None, None)
            .unwrap();
        assert!(
            res.scanned_fallback,
            "stale index must fall back to a full scan"
        );
        assert!(
            res.results.iter().any(|m| m.uid == "sym:2"),
            "a node added after the build must still be found"
        );
    }

    /// A generation bump without a count change (e.g. an in-place edit
    /// through the engine) must also mark the index stale.
    #[test]
    fn generation_bump_marks_trigram_index_stale() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        let store = store_with_text();
        store.build_trigram_index().unwrap();
        store.bump_graph_generation();
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(
            res.scanned_fallback,
            "a generation bump must stale the index"
        );
        assert!(
            !res.results.is_empty(),
            "the fallback scan still returns correct results"
        );
    }

    /// Postings built before provenance tracking (no `Meta` key) cannot
    /// be trusted — treat them as stale and scan.
    #[test]
    fn trigram_index_without_provenance_is_treated_as_stale() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        let store = store_with_text();
        store.build_trigram_index().unwrap();
        // Simulate a legacy index: drop the provenance row.
        let conn = store.conn().unwrap();
        conn.query("MATCH (m:Meta {key: 'trigram_index'}) DETACH DELETE m")
            .unwrap();
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(
            res.scanned_fallback,
            "postings without provenance must not be trusted"
        );
        assert!(
            !res.results.is_empty(),
            "the fallback scan still returns correct results"
        );
    }

    /// Observability: `stale_index` must report in-band whether a posting
    /// table existed but was bypassed as stale — false when no index was ever
    /// built, false for a fresh index, true once the graph drifts.
    #[test]
    fn stale_index_flag_reflects_index_state() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        // No index at all: fallback, but NOT stale (nothing to be stale).
        let store = store_with_text();
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(res.scanned_fallback);
        assert!(!res.stale_index, "no posting table → not stale");

        // Fresh index: pre-filter used, not stale.
        store.build_trigram_index().unwrap();
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(!res.scanned_fallback);
        assert!(!res.stale_index, "fresh index → not stale");

        // Graph drifts (in-place-edit style generation bump): stale.
        store.bump_graph_generation();
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(res.scanned_fallback, "stale index forces the full scan");
        assert!(res.stale_index, "bypassed stale index must be reported");

        // count_patterns reports the same flag per pattern.
        let counts = store
            .count_patterns(&["token".to_string()], None, None)
            .unwrap();
        assert!(
            counts[0].stale_index,
            "count_patterns must surface staleness per pattern"
        );
        // A fresh count is not stale.
        store.build_trigram_index().unwrap();
        let counts = store
            .count_patterns(&["token".to_string()], None, None)
            .unwrap();
        assert!(!counts[0].stale_index);
    }

    /// Observability: observing a fresh (non-stale) index re-arms the
    /// one-shot stale warning latch, so a long-lived daemon can warn again
    /// after a rebuild + restale instead of staying latched forever.
    #[test]
    fn fresh_index_observation_rearms_stale_warning_latch() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        let store = store_with_text();
        store.build_trigram_index().unwrap();

        // Latch the warning as if a stale index was already observed.
        TRIGRAM_STALE_WARNED.store(true, Ordering::Relaxed);
        // A fresh-index search must reset the latch.
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(!res.stale_index, "freshly built index must be trusted");
        assert!(
            !TRIGRAM_STALE_WARNED.load(Ordering::Relaxed),
            "observing a fresh index must re-arm the stale warning latch"
        );
        // Leave the latch clean for other tests in this process.
        TRIGRAM_STALE_WARNED.store(false, Ordering::Relaxed);
    }

    /// Unicode final sigma: `str::to_lowercase` folds 'Σ' context-sensitively
    /// ('ς' at a word end, 'σ' mid-word), so index-side trigrams (folded from
    /// a whole note body) and query-side trigrams (folded from a short
    /// literal) could disagree and the pre-filter would drop real matches.
    /// The per-char fold must keep both sides consistent.
    #[test]
    fn trigrams_use_context_free_case_folding() {
        // 'ΓΟΣ' standalone folds to final sigma under str::to_lowercase;
        // mid-word it folds to medial sigma. Per-char folding is stable.
        assert_eq!(
            trigrams("ΓΟΣ"),
            trigrams("γοσ"),
            "standalone uppercase must fold like its per-char lowercase form"
        );
        let midword = trigrams("ΓΟΣΑΝΘΡΑΞ");
        for tg in trigrams("ΓΟΣ") {
            assert!(
                midword.contains(&tg),
                "trigram {tg} of a standalone literal must also appear mid-word"
            );
        }
    }

    /// Unicode final sigma, end to end: a pattern whose literal is a word
    /// PREFIX in the indexed text must survive the trigram pre-filter.
    #[test]
    fn greek_final_sigma_prefilter_does_not_drop_matches() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&Note {
                uid: "note:v:greek".to_string(),
                vault_uid: "vlt:v".to_string(),
                file_path: "notes/greek.md".to_string(),
                title: "Greek".to_string(),
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
                uid: "sec:greek".to_string(),
                note_uid: "note:v:greek".to_string(),
                heading_uid: None,
                start_line: 1,
                end_line: 1,
                text_hash: "th".to_string(),
                // 'ΓΟΣ' appears only as a mid-word prefix here.
                text_content: "Η ΓΟΣΑΝΘΡΑΞ δοκιμή".to_string(),
                word_count: 3,
                pagerank_score: None,
            })
            .unwrap();
        store.build_trigram_index().unwrap();

        let res = store.regex_search("ΓΟΣ", None, None, None, None).unwrap();
        assert!(
            !res.scanned_fallback,
            "the literal pattern must use the pre-filter"
        );
        assert!(
            res.results.iter().any(|m| m.uid == "sec:greek"),
            "the mid-word prefix match must survive the pre-filter"
        );
    }

    /// LOW: `--limit 0` must return zero results — the limit is checked
    /// before a match is pushed, not after (previously one slipped through).
    #[test]
    fn limit_zero_returns_no_results() {
        let store = store_with_text();
        let res = store
            .regex_search("authenticateUser", None, None, Some(0), None)
            .unwrap();
        assert!(
            res.results.is_empty(),
            "limit 0 must return no results, got {:?}",
            res.results
        );
    }

    /// LOW: regex-search aligns with brain-search's presentation bound
    /// (`SEARCH_PRESENTATION_LIMIT_MAX`) instead of accepting any limit.
    #[test]
    fn limit_above_presentation_max_is_rejected() {
        let store = store_with_text();
        let err = store
            .regex_search(
                "token",
                None,
                None,
                Some(SEARCH_PRESENTATION_LIMIT_MAX + 1),
                None,
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("exceeds"),
            "unexpected error message: {err}"
        );
        assert!(
            store
                .regex_search(
                    "token",
                    None,
                    None,
                    Some(SEARCH_PRESENTATION_LIMIT_MAX),
                    None
                )
                .is_ok(),
            "the maximum itself must be accepted"
        );
    }

    /// G9: `location` must point at the match's line, not the node's start
    /// line, so plain-text output (which renders `location`) shows where the
    /// match actually is.
    #[test]
    fn location_points_at_match_line_not_section_start() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&Note {
                uid: "note:v:ml".to_string(),
                vault_uid: "vlt:v".to_string(),
                file_path: "notes/multi.md".to_string(),
                title: "Multi".to_string(),
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
                uid: "sec:v:ml".to_string(),
                note_uid: "note:v:ml".to_string(),
                heading_uid: None,
                start_line: 10,
                end_line: 12,
                text_hash: "th".to_string(),
                text_content: "first line\nsecond line has needle_word here\nthird line"
                    .to_string(),
                word_count: 9,
                pagerank_score: None,
            })
            .unwrap();

        let res = store
            .regex_search("needle_word", None, None, None, None)
            .unwrap();
        let hit = res
            .results
            .iter()
            .find(|m| m.uid == "sec:v:ml")
            .expect("section match present");
        // The match is on the section's second text line → file line 11, not
        // the section's start line 10.
        assert_eq!(hit.line, Some(11));
        assert_eq!(
            hit.location, "notes/multi.md:11",
            "location must carry the match line, not the section start line"
        );
    }
}
