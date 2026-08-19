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
//! (e.g. `.{4,}`), or for any repository/vault scope whose manifest is stale,
//! we fall back to scanning that scope's candidate text and running the
//! compiled regex against it. Ready scopes remain prefiltered. The trigram
//! pre-filter only ever *narrows* the candidate set — we always confirm with
//! the real regex.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use lbug::Value;
use regex_syntax::hir::literal::Extractor;
use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::StoreError;
use crate::regex_index::{
    REGEX_INDEX_SCHEMA_VERSION, REGEX_TOKENIZER_FINGERPRINT, RegexIndex, RegexShardDocument,
    RegexShardMetadata,
};
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

/// Legacy global provenance key. Its presence identifies the v1 positional
/// posting schema; the first v2 refresh migrates it atomically to stable,
/// scope-owned postings.
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
    /// True when any candidate scope was scanned directly (no usable literal,
    /// no posting table, or a mixed query containing dirty scopes).
    pub scanned_fallback: bool,
    /// True when persisted trigram postings exist but at least one scope was
    /// distrusted and scanned. This lets callers
    /// surface staleness in-band: on the daemon path the once-per-process
    /// stderr warning is invisible. Implies `scanned_fallback` when the
    /// pattern has usable literals; always false when no index was ever built.
    #[serde(default)]
    pub stale_index: bool,
    /// Number of repository/vault scopes safely narrowed by trigram postings.
    #[serde(default)]
    pub ready_scopes: usize,
    /// Number of scopes scanned directly because their postings were absent,
    /// stale, or mid-refresh. Correctness is preserved per scope.
    #[serde(default)]
    pub dirty_scopes: usize,
    /// Candidate nodes actually evaluated by the real regex.
    #[serde(default)]
    pub scanned_candidates: usize,
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
    #[serde(default)]
    pub ready_scopes: usize,
    #[serde(default)]
    pub dirty_scopes: usize,
    #[serde(default)]
    pub scanned_candidates: usize,
}

/// Work performed by an incremental trigram refresh.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrigramRefreshStats {
    pub scopes_refreshed: usize,
    pub scopes_unchanged: usize,
    pub nodes_added: usize,
    pub nodes_changed: usize,
    pub nodes_deleted: usize,
    pub postings_added: usize,
    pub postings_deleted: usize,
    pub migrated_legacy_index: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
}

/// A unit of indexed text plus its node metadata. Internal to this module.
#[derive(Clone)]
struct Candidate {
    uid: String,
    /// Repository UID for symbols, vault UID for notes and sections.
    scope_uid: String,
    /// Content-derived identity used to determine whether this node's
    /// postings need to change.
    text_hash: String,
    kind: String,
    title: String,
    location: String,
    text: String,
    /// 1-based line in the source file where this candidate's `text` begins.
    /// Used to translate a match's line *within* `text` into a file line.
    /// `1` when the node has no meaningful file offset (e.g. Note titles).
    start_line: u32,
}

#[derive(Clone)]
struct TrigramScopeState {
    status: String,
    candidate_count: usize,
    candidate_digest: String,
}

struct TrigramDocumentState {
    scope_uid: String,
    text_hash: String,
}

struct TrigramPrefilterPlan {
    matching_ready_uids: HashSet<String>,
    ready_scopes: HashSet<String>,
    dirty_scopes: HashSet<String>,
    has_index: bool,
}

#[derive(Clone)]
pub(crate) struct TrigramScopeCache {
    generation: u64,
    corpus_digest: String,
    ready_scopes: HashSet<String>,
    dirty_scopes: HashSet<String>,
    has_index: bool,
}

fn text_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn candidate_digest(candidates: &[Candidate]) -> String {
    let mut hasher = blake3::Hasher::new();
    for candidate in candidates {
        hasher.update(candidate.uid.as_bytes());
        hasher.update(&[0]);
        hasher.update(candidate.text_hash.as_bytes());
        hasher.update(&[0xff]);
    }
    hasher.finalize().to_hex().to_string()
}

fn stable_posting_uid(trigram: &str, node_uid: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(trigram.as_bytes());
    hasher.update(&[0]);
    hasher.update(node_uid.as_bytes());
    format!("tg:v2:{}", hasher.finalize().to_hex())
}

fn widened_scan_plan(
    ready_scopes: &HashSet<String>,
    dirty_scopes: &HashSet<String>,
    has_index: bool,
) -> TrigramPrefilterPlan {
    let mut all_dirty = dirty_scopes.clone();
    all_dirty.extend(ready_scopes.iter().cloned());
    TrigramPrefilterPlan {
        matching_ready_uids: HashSet::new(),
        ready_scopes: HashSet::new(),
        dirty_scopes: all_dirty,
        has_index,
    }
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
        if let Some(root) = self.regex_sidecar_root() {
            return root.join("scopes").is_dir();
        }
        let conn = match self.conn() {
            Ok(c) => c,
            Err(_) => return false,
        };
        match conn.query("MATCH (t:TrigramPosting) RETURN t.trigram LIMIT 1") {
            Ok(result) => result.count() > 0,
            Err(_) => false,
        }
    }

    /// Incrementally refresh trigram postings over all indexed text. Existing
    /// callers keep the historical return value (postings written), while
    /// [`GraphStore::refresh_trigram_index`] exposes detailed work metrics.
    pub fn build_trigram_index(&self) -> Result<usize, StoreError> {
        Ok(self.refresh_trigram_index(false)?.postings_added)
    }

    /// Force a one-time full rebuild using the stable v2 schema.
    pub fn rebuild_trigram_index(&self) -> Result<TrigramRefreshStats, StoreError> {
        self.refresh_trigram_index(true)
    }

    /// Refresh only scopes whose deterministic candidate digest changed. A
    /// scope is published `building` before mutation, then `ready` in the same
    /// transaction as its postings and document manifest. Search trusts ready
    /// scopes and scans only dirty scopes, so an interruption cannot lose
    /// matches outside (or inside) the failed scope.
    pub fn refresh_trigram_index(
        &self,
        force_full: bool,
    ) -> Result<TrigramRefreshStats, StoreError> {
        if self.regex_sidecar_root().is_some() {
            return self.refresh_regex_v3(force_full);
        }
        let refresh_started = Instant::now();
        *self
            .trigram_scope_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let candidates = self.collect_candidates(None, None)?;
        let mut desired: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for candidate in candidates {
            // Unknown ownership is never publishable as trusted. Search keeps
            // these candidates in the empty dirty scope and scans them.
            if candidate.scope_uid.is_empty() {
                continue;
            }
            desired
                .entry(candidate.scope_uid.clone())
                .or_default()
                .push(candidate);
        }
        for values in desired.values_mut() {
            values.sort_by(|a, b| a.uid.cmp(&b.uid));
        }

        let conn = self.conn()?;
        // Positional v1 IDs (`tg:<number>`) sort before versioned v2 IDs
        // (`tg:v2:<hash>`), so checking the first UID detects legacy rows
        // without scanning and materializing a potentially 12.9M-row table.
        let has_legacy_uid = conn
            .query("MATCH (t:TrigramPosting) RETURN t.uid ORDER BY t.uid LIMIT 1")
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .and_then(|row| row.first().cloned())
            .is_some_and(|value| match value {
                Value::String(uid) => !uid.starts_with("tg:v2:"),
                _ => true,
            });
        let legacy = has_legacy_uid
            || conn
                .query("MATCH (t:TrigramPosting) WHERE t.scope_uid = '' RETURN t.uid LIMIT 1")
                .map(|rows| rows.count() > 0)
                .unwrap_or(false)
            || conn
                .query(&format!(
                    "MATCH (m:Meta {{key: '{TRIGRAM_INDEX_META_KEY}'}}) RETURN m.value LIMIT 1"
                ))
                .map(|rows| rows.count() > 0)
                .unwrap_or(false);
        let mut stats = TrigramRefreshStats {
            migrated_legacy_index: legacy,
            ..Default::default()
        };
        if force_full || legacy {
            // Fail closed before any destructive full-rebuild statement. If
            // the process dies between the table clears below, every former
            // v2 scope remains visibly `building` and search scans it instead
            // of trusting a partially cleared posting set.
            for (scope_uid, scope) in self.read_trigram_scopes()? {
                Self::write_trigram_scope(
                    &conn,
                    &scope_uid,
                    "building",
                    scope.candidate_count,
                    &scope.candidate_digest,
                    self.graph_generation(),
                )?;
            }
            // A concurrent query can populate the cache in the interval
            // between the initial invalidation and the first durable
            // `building` marker. Invalidate again after every incumbent scope
            // is fail-closed and before deleting any postings, so a forced
            // rebuild (whose candidate corpus may be unchanged) cannot leave a
            // cached `ready` verdict trusting partial data after failure.
            *self
                .trigram_scope_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            stats.postings_deleted = conn
                .query("MATCH (t:TrigramPosting) RETURN count(t)")
                .ok()
                .and_then(|rows| rows.into_iter().next())
                .and_then(|row| row.first().cloned())
                .and_then(|value| match value {
                    Value::Int64(count) => Some(count.max(0) as usize),
                    _ => None,
                })
                .unwrap_or(0);
            conn.query("MATCH (t:TrigramPosting) DETACH DELETE t")
                .map_err(|e| StoreError::Query(format!("clear trigram postings: {e}")))?;
            conn.query("MATCH (d:TrigramDocument) DETACH DELETE d")
                .map_err(|e| StoreError::Query(format!("clear trigram documents: {e}")))?;
            // Keep the durable `building` rows written above. Missing scope
            // state is also fail-closed, but retaining the marker makes an
            // interrupted rebuild directly observable and lets each scope's
            // update transaction publish `ready` atomically.
            let _ = conn.query(&format!(
                "MATCH (m:Meta {{key: '{TRIGRAM_INDEX_META_KEY}'}}) DETACH DELETE m"
            ));
        }

        let existing_scopes = self.read_trigram_scopes()?;
        let mut all_scope_uids: HashSet<String> = desired.keys().cloned().collect();
        all_scope_uids.extend(existing_scopes.keys().cloned());
        let existing_docs = self.read_trigram_documents()?;
        let desired_scope_by_uid: HashMap<String, String> = desired
            .values()
            .flatten()
            .map(|candidate| (candidate.uid.clone(), candidate.scope_uid.clone()))
            .collect();

        let mut scope_uids: Vec<String> = all_scope_uids.into_iter().collect();
        scope_uids.sort();
        for scope_uid in scope_uids {
            let scope_candidates = desired.get(&scope_uid).map(Vec::as_slice).unwrap_or(&[]);
            let digest = candidate_digest(scope_candidates);
            let unchanged = !force_full
                && !legacy
                && existing_scopes.get(&scope_uid).is_some_and(|scope| {
                    scope.status == "ready"
                        && scope.candidate_count == scope_candidates.len()
                        && scope.candidate_digest == digest
                });
            if unchanged {
                stats.scopes_unchanged += 1;
                continue;
            }

            Self::write_trigram_scope(
                &conn,
                &scope_uid,
                "building",
                scope_candidates.len(),
                &digest,
                self.graph_generation(),
            )?;

            let old_for_scope: HashMap<String, String> = existing_docs
                .iter()
                .filter(|(_, doc)| doc.scope_uid == scope_uid)
                .map(|(uid, doc)| (uid.clone(), doc.text_hash.clone()))
                .collect();
            let deleted: Vec<String> = old_for_scope
                .keys()
                // A node that moved scopes is deleted and reinserted by its
                // destination scope. Treating it as deleted by the source
                // scope is order-dependent: if the destination ran first,
                // the source would erase the newly published postings.
                .filter(|uid| !desired_scope_by_uid.contains_key(uid.as_str()))
                .cloned()
                .collect();
            let changed: Vec<&Candidate> = scope_candidates
                .iter()
                .filter(|candidate| {
                    old_for_scope
                        .get(&candidate.uid)
                        .is_none_or(|hash| hash != &candidate.text_hash)
                })
                .collect();

            let txn = self.begin_transaction()?;
            let update = (|| -> Result<(usize, usize), StoreError> {
                let mut postings_deleted = 0usize;
                for uid in deleted
                    .iter()
                    .chain(changed.iter().map(|candidate| &candidate.uid))
                {
                    postings_deleted += Self::delete_trigram_document(&txn, uid)?;
                }
                let mut postings_added = 0usize;
                let mut insert_posting = txn
                    .prepare("CREATE (:TrigramPosting {uid: $uid, trigram: $trigram, node_uid: $node_uid, scope_uid: $scope_uid})")
                    .map_err(|e| StoreError::Query(format!("prepare trigram insert: {e}")))?;
                for candidate in &changed {
                    Self::insert_trigram_document(&txn, candidate)?;
                    for trigram in trigrams(&candidate.text) {
                        let posting_uid = stable_posting_uid(&trigram, &candidate.uid);
                        txn.execute(
                            &mut insert_posting,
                            vec![
                                ("uid", Value::String(posting_uid.clone())),
                                ("trigram", Value::String(trigram)),
                                ("node_uid", Value::String(candidate.uid.clone())),
                                ("scope_uid", Value::String(scope_uid.clone())),
                            ],
                        )
                        .map_err(|e| {
                            StoreError::Query(format!(
                                "insert stable trigram posting {posting_uid} (collision or duplicate): {e}"
                            ))
                        })?;
                        postings_added += 1;
                    }
                }
                if scope_candidates.is_empty() {
                    let mut delete_scope = txn
                        .prepare("MATCH (s:TrigramScope {uid: $uid}) DETACH DELETE s")
                        .map_err(|e| {
                            StoreError::Query(format!("prepare empty trigram scope delete: {e}"))
                        })?;
                    txn.execute(
                        &mut delete_scope,
                        vec![("uid", Value::String(scope_uid.clone()))],
                    )
                    .map_err(|e| StoreError::Query(format!("delete empty trigram scope: {e}")))?;
                } else {
                    Self::write_trigram_scope(
                        &txn,
                        &scope_uid,
                        "ready",
                        scope_candidates.len(),
                        &digest,
                        self.graph_generation(),
                    )?;
                }
                Ok((postings_added, postings_deleted))
            })();
            match update {
                Ok((added, deleted_postings)) => {
                    self.commit_transaction(&txn)?;
                    stats.scopes_refreshed += 1;
                    stats.postings_added += added;
                    stats.postings_deleted += deleted_postings;
                    stats.nodes_deleted += deleted.len();
                    for candidate in changed {
                        if existing_docs.contains_key(&candidate.uid) {
                            stats.nodes_changed += 1;
                        } else {
                            stats.nodes_added += 1;
                        }
                    }
                }
                Err(error) => {
                    let _ = self.rollback_transaction(&txn);
                    return Err(error);
                }
            }
        }
        *self
            .trigram_scope_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        stats.elapsed_ms = refresh_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        Ok(stats)
    }

    fn refresh_regex_v3(&self, force_full: bool) -> Result<TrigramRefreshStats, StoreError> {
        let started = Instant::now();
        let root = self.regex_sidecar_root().ok_or_else(|| {
            StoreError::Query("regex v3 requires an on-disk graph store".to_string())
        })?;
        let identity = self.publication_identity()?.ok_or_else(|| {
            StoreError::Query("regex v3 requires graph publication identity".to_string())
        })?;
        let index = RegexIndex::new(root);
        let mut desired: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for candidate in self.collect_candidates(None, None)? {
            if !candidate.scope_uid.is_empty() {
                desired
                    .entry(candidate.scope_uid.clone())
                    .or_default()
                    .push(candidate);
            }
        }
        for candidates in desired.values_mut() {
            candidates.sort_by(|left, right| left.uid.cmp(&right.uid));
        }

        let existing = index
            .list_metadata()
            .unwrap_or_default()
            .into_iter()
            .map(|metadata| (metadata.scope_uid.clone(), metadata))
            .collect::<HashMap<_, _>>();
        let mut stats = TrigramRefreshStats::default();

        let mut prior_documents: HashMap<String, (String, String)> = HashMap::new();
        for scope_uid in existing.keys() {
            if let Ok(Some(hashes)) = index.document_hashes(scope_uid) {
                for (uid, hash) in hashes {
                    prior_documents.insert(uid, (scope_uid.clone(), hash));
                }
            }
        }
        let desired_documents: HashMap<String, (String, String)> = desired
            .iter()
            .flat_map(|(scope_uid, candidates)| {
                candidates.iter().map(|candidate| {
                    (
                        candidate.uid.clone(),
                        (scope_uid.clone(), candidate.text_hash.clone()),
                    )
                })
            })
            .collect();
        stats.nodes_added = desired_documents
            .keys()
            .filter(|uid| !prior_documents.contains_key(uid.as_str()))
            .count();
        stats.nodes_changed = desired_documents
            .iter()
            .filter(|(uid, desired)| {
                prior_documents
                    .get(uid.as_str())
                    .is_some_and(|prior| prior != *desired)
            })
            .count();
        stats.nodes_deleted = prior_documents
            .keys()
            .filter(|uid| !desired_documents.contains_key(uid.as_str()))
            .count();
        for (scope_uid, candidates) in &desired {
            let digest = candidate_digest(candidates);
            let prior = existing.get(scope_uid);
            let compatible = prior.is_some_and(|metadata| {
                metadata.schema_version == REGEX_INDEX_SCHEMA_VERSION
                    && metadata.tokenizer_fingerprint == REGEX_TOKENIZER_FINGERPRINT
                    && metadata.brain_uuid == identity.brain_uuid
                    && metadata.publication_uuid == identity.publication_uuid
                    && metadata.candidate_count == candidates.len()
                    && metadata.candidate_digest == digest
            });
            if compatible && !force_full {
                stats.scopes_unchanged += 1;
                continue;
            }

            let scope_epoch = prior.map_or(1, |metadata| metadata.scope_epoch.saturating_add(1));
            let metadata = RegexShardMetadata {
                schema_version: REGEX_INDEX_SCHEMA_VERSION,
                tokenizer_fingerprint: REGEX_TOKENIZER_FINGERPRINT.to_string(),
                brain_uuid: identity.brain_uuid.clone(),
                publication_uuid: identity.publication_uuid.clone(),
                source_graph_generation: self.graph_generation(),
                scope_uid: scope_uid.clone(),
                scope_epoch,
                candidate_count: candidates.len(),
                candidate_digest: digest,
            };
            let trigram_sets: Vec<HashSet<String>> = candidates
                .iter()
                .map(|candidate| trigrams(&candidate.text))
                .collect();
            let documents: Vec<RegexShardDocument<'_>> = candidates
                .iter()
                .zip(&trigram_sets)
                .map(|(candidate, trigrams)| RegexShardDocument {
                    uid: &candidate.uid,
                    kind: &candidate.kind,
                    text_hash: &candidate.text_hash,
                    trigrams,
                })
                .collect();
            index.replace_scope(metadata, &documents)?;

            stats.scopes_refreshed += 1;
            stats.postings_added += trigram_sets.iter().map(HashSet::len).sum::<usize>();
        }
        for scope_uid in existing.keys() {
            if !desired.contains_key(scope_uid) && index.retire_scope(scope_uid)? {
                stats.scopes_refreshed += 1;
            }
        }
        stats.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        *self
            .trigram_scope_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(stats)
    }

    fn write_trigram_scope(
        conn: &lbug::Connection<'_>,
        uid: &str,
        status: &str,
        candidate_count: usize,
        candidate_digest: &str,
        indexed_generation: u64,
    ) -> Result<(), StoreError> {
        let mut delete = conn
            .prepare("MATCH (s:TrigramScope {uid: $uid}) DETACH DELETE s")
            .map_err(|e| StoreError::Query(format!("prepare trigram scope delete: {e}")))?;
        conn.execute(&mut delete, vec![("uid", Value::String(uid.to_string()))])
            .map_err(|e| StoreError::Query(format!("delete trigram scope: {e}")))?;
        let mut insert = conn
            .prepare("CREATE (:TrigramScope {uid: $uid, status: $status, candidate_count: $count, candidate_digest: $digest, indexed_generation: $generation})")
            .map_err(|e| StoreError::Query(format!("prepare trigram scope insert: {e}")))?;
        conn.execute(
            &mut insert,
            vec![
                ("uid", Value::String(uid.to_string())),
                ("status", Value::String(status.to_string())),
                ("count", Value::Int64(candidate_count as i64)),
                ("digest", Value::String(candidate_digest.to_string())),
                (
                    "generation",
                    Value::Int64(indexed_generation.min(i64::MAX as u64) as i64),
                ),
            ],
        )
        .map_err(|e| StoreError::Query(format!("write trigram scope: {e}")))?;
        Ok(())
    }

    fn delete_trigram_document(
        conn: &lbug::Connection<'_>,
        node_uid: &str,
    ) -> Result<usize, StoreError> {
        let mut count = conn
            .prepare("MATCH (t:TrigramPosting {node_uid: $uid}) RETURN count(t)")
            .map_err(|e| StoreError::Query(format!("prepare trigram posting count: {e}")))?;
        let rows = conn
            .execute(
                &mut count,
                vec![("uid", Value::String(node_uid.to_string()))],
            )
            .map_err(|e| StoreError::Query(format!("count trigram postings: {e}")))?;
        let posting_count = rows
            .into_iter()
            .next()
            .and_then(|row| row.first().cloned())
            .and_then(|value| match value {
                Value::Int64(count) => Some(count.max(0) as usize),
                _ => None,
            })
            .unwrap_or(0);
        let mut delete_postings = conn
            .prepare("MATCH (t:TrigramPosting {node_uid: $uid}) DETACH DELETE t")
            .map_err(|e| StoreError::Query(format!("prepare trigram posting delete: {e}")))?;
        conn.execute(
            &mut delete_postings,
            vec![("uid", Value::String(node_uid.to_string()))],
        )
        .map_err(|e| StoreError::Query(format!("delete trigram postings: {e}")))?;
        let mut delete_doc = conn
            .prepare("MATCH (d:TrigramDocument {uid: $uid}) DETACH DELETE d")
            .map_err(|e| StoreError::Query(format!("prepare trigram document delete: {e}")))?;
        conn.execute(
            &mut delete_doc,
            vec![("uid", Value::String(node_uid.to_string()))],
        )
        .map_err(|e| StoreError::Query(format!("delete trigram document: {e}")))?;
        Ok(posting_count)
    }

    fn insert_trigram_document(
        conn: &lbug::Connection<'_>,
        candidate: &Candidate,
    ) -> Result<(), StoreError> {
        let mut insert = conn
            .prepare("CREATE (:TrigramDocument {uid: $uid, scope_uid: $scope_uid, text_hash: $text_hash})")
            .map_err(|e| StoreError::Query(format!("prepare trigram document insert: {e}")))?;
        conn.execute(
            &mut insert,
            vec![
                ("uid", Value::String(candidate.uid.clone())),
                ("scope_uid", Value::String(candidate.scope_uid.clone())),
                ("text_hash", Value::String(candidate.text_hash.clone())),
            ],
        )
        .map_err(|e| StoreError::Query(format!("insert trigram document: {e}")))?;
        Ok(())
    }

    fn read_trigram_scopes(&self) -> Result<HashMap<String, TrigramScopeState>, StoreError> {
        let conn = match self.conn() {
            Ok(conn) => conn,
            Err(_) => return Ok(HashMap::new()),
        };
        let rows = match conn.query(
            "MATCH (s:TrigramScope) RETURN s.uid, s.status, s.candidate_count, s.candidate_digest",
        ) {
            Ok(rows) => rows,
            // A read-only process can open a v1 database before a writer has
            // run the schema migration. Missing/unreadable scope state means
            // trust nothing and scan, never fail the correctness path.
            Err(_) => return Ok(HashMap::new()),
        };
        let mut scopes = HashMap::new();
        for row in rows {
            if let [
                Value::String(uid),
                Value::String(status),
                Value::Int64(count),
                Value::String(digest),
            ] = row.as_slice()
            {
                scopes.insert(
                    uid.clone(),
                    TrigramScopeState {
                        status: status.clone(),
                        candidate_count: (*count).max(0) as usize,
                        candidate_digest: digest.clone(),
                    },
                );
            }
        }
        Ok(scopes)
    }

    fn read_trigram_documents(&self) -> Result<HashMap<String, TrigramDocumentState>, StoreError> {
        let conn = self.conn()?;
        let rows = conn
            .query("MATCH (d:TrigramDocument) RETURN d.uid, d.scope_uid, d.text_hash")
            .map_err(|e| StoreError::Query(format!("read trigram documents: {e}")))?;
        let mut docs = HashMap::new();
        for row in rows {
            if let [
                Value::String(uid),
                Value::String(scope_uid),
                Value::String(text_hash),
            ] = row.as_slice()
            {
                docs.insert(
                    uid.clone(),
                    TrigramDocumentState {
                        scope_uid: scope_uid.clone(),
                        text_hash: text_hash.clone(),
                    },
                );
            }
        }
        Ok(docs)
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
            let note_path: HashMap<String, (String, String)> = notes
                .into_iter()
                .map(|n| (n.uid, (n.file_path, n.vault_uid)))
                .collect();
            for s in self.list_all_sections()? {
                if s.text_content.is_empty() {
                    continue;
                }
                let (path, scope_uid) = note_path.get(&s.note_uid).cloned().unwrap_or_default();
                let location = format!("{path}:{}", s.start_line);
                if let Some(prefix) = path_prefix
                    && !path.starts_with(prefix)
                {
                    continue;
                }
                out.push(Candidate {
                    uid: s.uid,
                    scope_uid,
                    text_hash: if s.text_hash.is_empty() {
                        text_hash(&s.text_content)
                    } else {
                        s.text_hash
                    },
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
                    scope_uid: n.vault_uid,
                    text_hash: text_hash(&n.title),
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
                    scope_uid: sym.repo_uid,
                    text_hash: text_hash(&sym.signature),
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
        all_candidates: &[Candidate],
    ) -> Result<TrigramPrefilterPlan, StoreError> {
        if self.regex_sidecar_root().is_some() {
            return self.regex_v3_candidate_uids(clauses, all_candidates);
        }
        let mut by_scope: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for candidate in all_candidates {
            by_scope
                .entry(candidate.scope_uid.clone())
                .or_default()
                .push(candidate.clone());
        }
        for values in by_scope.values_mut() {
            values.sort_by(|a, b| a.uid.cmp(&b.uid));
        }
        let mut corpus_hasher = blake3::Hasher::new();
        for (scope_uid, candidates) in &by_scope {
            corpus_hasher.update(scope_uid.as_bytes());
            corpus_hasher.update(&[0]);
            corpus_hasher.update(candidate_digest(candidates).as_bytes());
            corpus_hasher.update(&[0xff]);
        }
        let corpus_digest = corpus_hasher.finalize().to_hex().to_string();
        let generation = self.graph_generation();
        let cached = self
            .trigram_scope_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .filter(|cache| cache.generation == generation && cache.corpus_digest == corpus_digest);
        let (ready_scopes, dirty_scopes, has_index) = if let Some(cache) = cached {
            (cache.ready_scopes, cache.dirty_scopes, cache.has_index)
        } else {
            let stored_scopes = self.read_trigram_scopes()?;
            let has_index = self.has_trigram_index() || !stored_scopes.is_empty();
            let mut ready_scopes = HashSet::new();
            let mut dirty_scopes = HashSet::new();
            for (scope_uid, candidates) in &by_scope {
                let digest = candidate_digest(candidates);
                if stored_scopes.get(scope_uid).is_some_and(|scope| {
                    scope.status == "ready"
                        && scope.candidate_count == candidates.len()
                        && scope.candidate_digest == digest
                }) {
                    ready_scopes.insert(scope_uid.clone());
                } else {
                    dirty_scopes.insert(scope_uid.clone());
                }
            }
            for scope_uid in stored_scopes.keys() {
                if !ready_scopes.contains(scope_uid) && !dirty_scopes.contains(scope_uid) {
                    dirty_scopes.insert(scope_uid.clone());
                }
            }
            *self
                .trigram_scope_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(TrigramScopeCache {
                generation,
                corpus_digest,
                ready_scopes: ready_scopes.clone(),
                dirty_scopes: dirty_scopes.clone(),
                has_index,
            });
            (ready_scopes, dirty_scopes, has_index)
        };

        if has_index
            && !dirty_scopes.is_empty()
            && !TRIGRAM_STALE_WARNED.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "warning: {} trigram scope(s) are stale; scanning only those scopes — rerun `index --with-trigrams` to refresh them",
                dirty_scopes.len()
            );
        } else if dirty_scopes.is_empty() {
            TRIGRAM_STALE_WARNED.store(false, Ordering::Relaxed);
        }

        if ready_scopes.is_empty() {
            return Ok(TrigramPrefilterPlan {
                matching_ready_uids: HashSet::new(),
                ready_scopes,
                dirty_scopes,
                has_index,
            });
        }

        let conn = match self.conn() {
            Ok(conn) => conn,
            Err(_) => {
                return Ok(widened_scan_plan(&ready_scopes, &dirty_scopes, has_index));
            }
        };
        let mut acc: Option<HashSet<String>> = None;
        for clause in clauses {
            let mut clause_uids = HashSet::new();
            for tg in clause {
                let mut stmt = match conn.prepare(
                    "MATCH (t:TrigramPosting {trigram: $tg}) RETURN t.node_uid, t.scope_uid",
                ) {
                    Ok(stmt) => stmt,
                    Err(_) => {
                        return Ok(widened_scan_plan(&ready_scopes, &dirty_scopes, has_index));
                    }
                };
                let rows = match conn.execute(&mut stmt, vec![("tg", Value::String(tg.clone()))]) {
                    Ok(rows) => rows,
                    Err(_) => {
                        return Ok(widened_scan_plan(&ready_scopes, &dirty_scopes, has_index));
                    }
                };
                for row in rows {
                    if let [Value::String(uid), Value::String(scope_uid)] = row.as_slice()
                        && ready_scopes.contains(scope_uid)
                    {
                        clause_uids.insert(uid.clone());
                    }
                }
            }
            acc = Some(match acc {
                None => clause_uids,
                Some(previous) => previous.intersection(&clause_uids).cloned().collect(),
            });
            if acc.as_ref().is_some_and(HashSet::is_empty) {
                break;
            }
        }
        Ok(TrigramPrefilterPlan {
            matching_ready_uids: acc.unwrap_or_default(),
            ready_scopes,
            dirty_scopes,
            has_index,
        })
    }

    fn regex_v3_candidate_uids(
        &self,
        clauses: &[HashSet<String>],
        all_candidates: &[Candidate],
    ) -> Result<TrigramPrefilterPlan, StoreError> {
        let Some(root) = self.regex_sidecar_root() else {
            return Ok(TrigramPrefilterPlan {
                matching_ready_uids: HashSet::new(),
                ready_scopes: HashSet::new(),
                dirty_scopes: all_candidates
                    .iter()
                    .map(|candidate| candidate.scope_uid.clone())
                    .collect(),
                has_index: false,
            });
        };
        let index = RegexIndex::new(root);
        let identity = self.publication_identity()?.ok_or_else(|| {
            StoreError::Query("regex v3 requires graph publication identity".to_string())
        })?;
        let mut by_scope: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
        for candidate in all_candidates {
            by_scope
                .entry(candidate.scope_uid.clone())
                .or_default()
                .push(candidate.clone());
        }
        for candidates in by_scope.values_mut() {
            candidates.sort_by(|left, right| left.uid.cmp(&right.uid));
        }

        let has_index = index.root().join("scopes").is_dir();
        let mut ready_scopes = HashSet::new();
        let mut dirty_scopes = HashSet::new();
        let mut matching_ready_uids = HashSet::new();
        for (scope_uid, candidates) in by_scope {
            if scope_uid.is_empty() {
                dirty_scopes.insert(scope_uid);
                continue;
            }
            let metadata = match index.metadata(&scope_uid) {
                Ok(Some(metadata)) => metadata,
                Ok(None) | Err(_) => {
                    dirty_scopes.insert(scope_uid);
                    continue;
                }
            };
            let trusted = metadata.schema_version == REGEX_INDEX_SCHEMA_VERSION
                && metadata.tokenizer_fingerprint == REGEX_TOKENIZER_FINGERPRINT
                && metadata.brain_uuid == identity.brain_uuid
                && metadata.publication_uuid == identity.publication_uuid
                && metadata.candidate_count == candidates.len()
                && metadata.candidate_digest == candidate_digest(&candidates);
            if !trusted {
                dirty_scopes.insert(scope_uid);
                continue;
            }
            match index.candidate_uids(&metadata, clauses, CANDIDATE_CAP) {
                Ok(Some(uids)) => {
                    ready_scopes.insert(scope_uid);
                    matching_ready_uids.extend(uids);
                }
                Ok(None) | Err(_) => {
                    dirty_scopes.insert(scope_uid);
                }
            }
        }

        if has_index
            && !dirty_scopes.is_empty()
            && !TRIGRAM_STALE_WARNED.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "warning: {} regex shard(s) are unavailable or stale; scanning only those scopes — rerun `index --with-trigrams` to repair them",
                dirty_scopes.len()
            );
        } else if dirty_scopes.is_empty() {
            TRIGRAM_STALE_WARNED.store(false, Ordering::Relaxed);
        }
        Ok(TrigramPrefilterPlan {
            matching_ready_uids,
            ready_scopes,
            dirty_scopes,
            has_index,
        })
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

        // Validate scope manifests against the complete corpus before applying
        // request filters. A path/kind subset must never make a healthy scope
        // appear stale merely because candidates were intentionally omitted.
        let all_candidates = self.collect_candidates(None, None)?;
        let clauses = required_trigram_clauses(pattern);
        let plan = match &clauses {
            Some(clauses) => Some(self.trigram_candidate_uids(clauses, &all_candidates)?),
            None => None,
        };
        let scanned_fallback = plan
            .as_ref()
            .is_none_or(|plan| !plan.dirty_scopes.is_empty());
        let stale_index = plan
            .as_ref()
            .is_some_and(|plan| plan.has_index && !plan.dirty_scopes.is_empty());
        let ready_scopes = plan.as_ref().map_or(0, |plan| plan.ready_scopes.len());
        let dirty_scopes = plan.as_ref().map_or(0, |plan| plan.dirty_scopes.len());

        let mut candidates = self.collect_candidates(path_prefix, kinds)?;
        if let Some(plan) = &plan {
            candidates.retain(|candidate| {
                plan.dirty_scopes.contains(&candidate.scope_uid)
                    || (plan.ready_scopes.contains(&candidate.scope_uid)
                        && plan.matching_ready_uids.contains(&candidate.uid))
            });
        }

        // Scan the full candidate set, bounded by the wall-clock deadline (and a
        // high safety ceiling) — NOT a low pre-truncation. `truncated` is set
        // ONLY when the scan actually stops early, so `truncated:true` with an
        // empty `results` now genuinely means "incomplete scan" rather than
        // "the match was ordered past a 5000 cap and never scanned" (nw-076).
        let mut truncated = false;
        let mut results = Vec::new();
        let mut scanned_candidates = 0usize;
        for (i, c) in candidates.iter().enumerate() {
            if start.elapsed().as_millis() as u64 > deadline_ms || i >= CANDIDATE_CAP {
                truncated = true;
                break;
            }
            scanned_candidates += 1;
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
            ready_scopes,
            dirty_scopes,
            scanned_candidates,
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
        // Scope trust is always verified against the complete corpus. Request
        // filters select results only; they must not make omitted candidates
        // look like scope drift and disable otherwise healthy postings.
        let corpus_candidates = self.collect_candidates(None, None)?;
        let filtered_candidates = self.collect_candidates(path_prefix, kinds)?;

        let mut out = Vec::new();
        for pattern in patterns {
            let re = compile_pattern(pattern)?;

            // Optional trigram narrowing.
            let clauses = required_trigram_clauses(pattern);
            let plan = match &clauses {
                Some(clauses) => Some(self.trigram_candidate_uids(clauses, &corpus_candidates)?),
                None => None,
            };
            let stale_index = plan
                .as_ref()
                .is_some_and(|plan| plan.has_index && !plan.dirty_scopes.is_empty());

            let mut per_file: HashMap<String, u64> = HashMap::new();
            let mut total: u64 = 0;
            let mut scanned_candidates = 0usize;
            for c in &filtered_candidates {
                if let Some(plan) = &plan {
                    let should_scan = plan.dirty_scopes.contains(&c.scope_uid)
                        || (plan.ready_scopes.contains(&c.scope_uid)
                            && plan.matching_ready_uids.contains(&c.uid));
                    if !should_scan {
                        continue;
                    }
                }
                scanned_candidates += 1;
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
                ready_scopes: plan.as_ref().map_or(0, |plan| plan.ready_scopes.len()),
                dirty_scopes: plan.as_ref().map_or(0, |plan| plan.dirty_scopes.len()),
                scanned_candidates,
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
            ready_scopes: 0,
            dirty_scopes: 0,
            scanned_candidates: 0,
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
            ready_scopes: 0,
            dirty_scopes: 0,
            scanned_candidates: 0,
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
            ready_scopes: 0,
            dirty_scopes: 0,
            scanned_candidates: 0,
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
        populate_store(&store);
        store
    }

    fn populate_store(store: &GraphStore) {
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
    }

    #[test]
    fn on_disk_store_uses_identity_bound_regex_v3_shards() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("brain.lbug");
        let store = GraphStore::open(&db).unwrap();
        populate_store(&store);

        let first = store.rebuild_trigram_index().unwrap();
        assert_eq!(first.scopes_refreshed, 2);
        assert!(first.postings_added > 0);
        let root = store.regex_sidecar_root().unwrap();
        assert!(root.join("scopes").is_dir());
        let conn = store.conn().unwrap();
        assert!(
            conn.query("MATCH (t:TrigramPosting) RETURN t.uid LIMIT 1")
                .is_err()
        );
        assert!(
            conn.query("MATCH (d:TrigramDocument) RETURN d.uid LIMIT 1")
                .is_err()
        );

        let result = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(!result.scanned_fallback);
        assert_eq!(result.ready_scopes, 2);
        assert_eq!(result.dirty_scopes, 0);
        assert_eq!(result.results.len(), 2);

        let second = store.refresh_trigram_index(false).unwrap();
        assert_eq!(second.scopes_refreshed, 0);
        assert_eq!(second.scopes_unchanged, 2);

        let identity = store.publication_identity().unwrap().unwrap();
        let metadata = RegexIndex::new(root).list_metadata().unwrap();
        assert_eq!(metadata.len(), 2);
        assert!(metadata.iter().all(|entry| {
            entry.brain_uuid == identity.brain_uuid
                && entry.publication_uuid == identity.publication_uuid
                && entry.schema_version == REGEX_INDEX_SCHEMA_VERSION
        }));
    }

    #[test]
    fn corrupt_regex_v3_shard_widens_only_that_scope_to_scan() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("brain.lbug");
        let store = GraphStore::open(&db).unwrap();
        populate_store(&store);
        store.rebuild_trigram_index().unwrap();

        let scopes = store.regex_sidecar_root().unwrap().join("scopes");
        let scope_dir = std::fs::read_dir(&scopes)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let current = scope_dir.join("CURRENT");
        let mut bytes = std::fs::read(&current).unwrap();
        bytes.extend_from_slice(b"corrupt");
        std::fs::write(&current, bytes).unwrap();

        let result = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(result.scanned_fallback);
        assert!(result.stale_index);
        assert_eq!(result.ready_scopes, 1);
        assert_eq!(result.dirty_scopes, 1);
        let uids: HashSet<&str> = result.results.iter().map(|hit| hit.uid.as_str()).collect();
        assert!(uids.contains("sec:v:1:a"));
        assert!(uids.contains("sym:1"));
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
    fn count_filters_do_not_make_complete_scopes_look_stale() {
        let store = store_with_text();
        store.build_trigram_index().unwrap();
        let counts = store
            .count_patterns(
                &["authenticateUser".to_string()],
                Some("src/"),
                Some(&["Symbol".to_string()]),
            )
            .unwrap();
        assert_eq!(counts[0].total_matches, 1);
        assert!(!counts[0].stale_index);
        assert_eq!(counts[0].dirty_scopes, 0);
        assert_eq!(counts[0].scanned_candidates, 1);
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

    #[test]
    fn stable_posting_ids_are_order_independent_and_tuple_sensitive() {
        let first = stable_posting_uid("abc", "sym:one");
        assert_eq!(first, stable_posting_uid("abc", "sym:one"));
        assert_ne!(first, stable_posting_uid("abd", "sym:one"));
        assert_ne!(first, stable_posting_uid("abc", "sym:two"));
        assert!(first.starts_with("tg:v2:"));
    }

    #[test]
    fn graph_posting_tables_are_absent_and_refresh_builds_regex_v3() {
        let store = store_with_text();
        let conn = store.conn().unwrap();
        assert!(conn.query("MATCH (t:TrigramPosting) RETURN t.uid").is_err());
        assert!(
            conn.query("MATCH (d:TrigramDocument) RETURN d.uid")
                .is_err()
        );
        let first = store.refresh_trigram_index(false).unwrap();
        assert!(!first.migrated_legacy_index);
        assert!(first.scopes_refreshed >= 2);
        let second = store.refresh_trigram_index(false).unwrap();
        assert!(!second.migrated_legacy_index);
        assert_eq!(second.scopes_refreshed, 0);
    }

    #[test]
    fn dirty_repo_is_scanned_while_unrelated_scopes_keep_prefiltering() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        let store = store_with_text();
        let symbol = |uid: &str, repo_uid: &str, signature: &str| Symbol {
            uid: uid.into(),
            name: uid.into(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.into(),
            file_path: format!("src/{uid}.rs"),
            start_line: 1,
            end_line: 1,
            signature: signature.into(),
            summary: None,
            content_hash: text_hash(signature),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store
            .insert_symbol(&symbol("sym:repo2", "repo:2", "fn sharedScopeMarker()"))
            .unwrap();
        let first = store.refresh_trigram_index(false).unwrap();
        assert!(first.scopes_refreshed >= 3, "repo 1, repo 2, and vault");

        // Only repo:1 changes after publication.
        store
            .insert_symbol(&symbol("sym:repo1:new", "repo:1", "fn sharedScopeMarker()"))
            .unwrap();
        let result = store
            .regex_search("sharedScopeMarker", None, None, None, None)
            .unwrap();
        let uids: HashSet<_> = result.results.iter().map(|hit| hit.uid.as_str()).collect();
        assert!(uids.contains("sym:repo2"), "ready repo uses postings");
        assert!(uids.contains("sym:repo1:new"), "dirty repo is scanned");
        assert!(
            result.scanned_fallback,
            "mixed query includes a scoped scan"
        );
        assert!(result.stale_index);
        assert!(result.ready_scopes >= 2);
        assert_eq!(result.dirty_scopes, 1);

        let refresh = store.refresh_trigram_index(false).unwrap();
        assert_eq!(refresh.scopes_refreshed, 1);
        assert!(refresh.scopes_unchanged >= 2);
        assert_eq!(refresh.nodes_added, 1);
        let unchanged = store.refresh_trigram_index(false).unwrap();
        assert_eq!(unchanged.scopes_refreshed, 0);
        assert_eq!(unchanged.postings_added, 0);
        assert!(unchanged.scopes_unchanged >= 3);
    }

    #[test]
    fn moving_a_node_to_an_earlier_scope_cannot_erase_new_postings() {
        let store = store_with_text();
        store.refresh_trigram_index(false).unwrap();

        // `repo:0` sorts before the incumbent `repo:1`. This exercises the
        // dangerous order where the destination publishes first and the
        // source scope is cleaned afterward.
        store
            .conn()
            .unwrap()
            .query("MATCH (s:Symbol {uid: 'sym:1'}) SET s.repo_uid = 'repo:0'")
            .unwrap();
        let refresh = store.refresh_trigram_index(false).unwrap();
        assert_eq!(refresh.nodes_changed, 1);
        assert_eq!(refresh.nodes_deleted, 0);

        let result = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(
            result.results.iter().any(|hit| hit.uid == "sym:1"),
            "the moved symbol must remain reachable through its new postings"
        );
        assert!(
            !result.scanned_fallback,
            "both source cleanup and destination publication completed"
        );
        let scopes = RegexIndex::new(store.regex_sidecar_root().unwrap())
            .list_metadata()
            .unwrap()
            .into_iter()
            .map(|metadata| metadata.scope_uid)
            .collect::<HashSet<_>>();
        assert!(scopes.contains("repo:0"));
        assert!(!scopes.contains("repo:1"));
    }

    /// A generation bump with identical searchable digests must not cause any
    /// scope to lose trust or rewrite postings.
    #[test]
    fn generation_bump_with_identical_digests_keeps_scopes_ready() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        let store = store_with_text();
        store.build_trigram_index().unwrap();
        store.bump_graph_generation();
        let res = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(!res.scanned_fallback);
        assert!(!res.stale_index);
        assert!(
            !res.results.is_empty(),
            "the fallback scan still returns correct results"
        );
    }

    /// A shard generation without its durable selector cannot be trusted.
    #[test]
    fn regex_generation_without_current_pointer_is_treated_as_stale() {
        let _latch_guard = LATCH_TEST_LOCK.lock().unwrap();
        let store = store_with_text();
        store.build_trigram_index().unwrap();
        // Keep the immutable generation but remove one scope's durable
        // selector, simulating a crash before pointer publication.
        let scopes = store.regex_sidecar_root().unwrap().join("scopes");
        let scope = std::fs::read_dir(scopes)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(scope.join("CURRENT")).unwrap();
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

        // Searchable content drifts in the repository scope: stale.
        store
            .insert_symbol(&Symbol {
                uid: "sym:drift".into(),
                name: "drift".into(),
                kind: SymbolKind::Function,
                repo_uid: "repo:1".into(),
                file_path: "src/drift.rs".into(),
                start_line: 1,
                end_line: 1,
                signature: "fn drift()".into(),
                summary: None,
                content_hash: "drift".into(),
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
