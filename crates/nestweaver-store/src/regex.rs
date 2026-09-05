//! Trigram-accelerated first-party regex search over indexed text.
//!
//! NestWeaver already stores searchable text in the graph: `Section.text_content`
//! (markdown brain bodies), `Note.title`, and `Symbol.signature`. This module
//! lets agents run a real `regex` against that text without shelling out to
//! `rg`/`grep`, with a disposable per-scope Tantivy trigram accelerator.
//!
//! ## Correctness vs. optimization
//!
//! The regex sidecar is purely an optimization. Correctness never depends on
//! it: when no shard exists, when the pattern yields no usable literal trigrams
//! (e.g. `.{4,}`), or for any repository/vault scope whose manifest is stale,
//! we fall back to scanning that scope's candidate text and running the
//! compiled regex against it. Ready scopes remain prefiltered. The trigram
//! pre-filter only ever *narrows* the candidate set — we always confirm with
//! the real regex.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use lbug::Value;
use regex_syntax::hir::literal::Extractor;
use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::{CancelReason, StoreError};
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

/// Do not start a graph-hydration phase when less than this budget remains.
/// Ladybug queries cannot be interrupted mid-call, so admission control keeps
/// tiny deadlines from launching work that is already certain to outlive them.
const PHASE_ADMISSION_MILLIS: u64 = 5;

/// Maximum accepted regex pattern length, in bytes. A longer pattern is rejected
/// before compilation so an untrusted client cannot force a large compile just
/// by sending a huge pattern.
pub const MAX_PATTERN_BYTES: usize = 4096;

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

/// File line of a frontmatter block's FIRST content line.
///
/// Line 1 is the opening `---` fence, so the YAML itself starts at 2. The
/// markdown parser already shifts every heading/section/wikilink line to be
/// file-absolute, so a frontmatter candidate anchored here keeps `regex_search`
/// line numbers honest across the whole file (nw-298).
const FRONTMATTER_START_LINE: u32 = 2;

/// A single regex match hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegexMatch {
    /// Node UID (sym:..., sec:..., note:...).
    pub uid: String,
    /// Node kind discriminator: "Symbol", "Section", "Note" or "Frontmatter".
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
    /// Scopes whose accelerator could not be opened or queried. These are a
    /// subset of `dirty_scopes` and were safely scanned instead.
    #[serde(default)]
    pub error_scopes: usize,
    /// Unique candidate UIDs returned by trusted posting lists before graph
    /// hydration.
    #[serde(default)]
    pub posting_hits: usize,
    /// Posting hits that still existed in the graph and passed filters.
    #[serde(default)]
    pub hydrated_candidates: usize,
    /// Candidate nodes actually evaluated by the real regex.
    #[serde(default)]
    pub scanned_candidates: usize,
    /// Exact reason the result set is partial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<RegexTruncationReason>,
    /// Query-stage wall times for operational diagnosis.
    #[serde(default)]
    pub timings: RegexStageTimings,
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegexTruncationReason {
    ResultLimit,
    Deadline,
    CandidateCap,
    /// The candidate corpus itself was short: a whole-corpus scan reached rows
    /// it could not decode and dropped them (nw-335 corrupt-row tolerance).
    ///
    /// This module's contract is that an incomplete answer SAYS it is
    /// incomplete — `Deadline` and `CandidateCap` exist for exactly that, and
    /// nw-076 was this surface reporting a partial scan as definitive. A
    /// tolerated corrupt row bypassed both: the corpus silently shrank and the
    /// result still claimed `truncated: false`. "No matches" over a corpus with
    /// unread rows is not a verified absence.
    UndecodableRows,
    #[default]
    Unknown,
}

/// Pick the reason a caller can act on when two stages each reported one.
///
/// [`RegexTruncationReason::UndecodableRows`] describes the CORPUS, not the
/// scan, so any genuine stop (deadline, cap, limit) outranks it — but it must
/// survive when nothing else stopped, or a short corpus reports as complete.
fn stronger_truncation(
    first: Option<RegexTruncationReason>,
    second: Option<RegexTruncationReason>,
) -> Option<RegexTruncationReason> {
    match (first, second) {
        (Some(RegexTruncationReason::UndecodableRows), Some(other)) => Some(other),
        (Some(first), _) => Some(first),
        (None, second) => second,
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegexStageTimings {
    pub planning_ms: u64,
    pub hydration_ms: u64,
    pub verification_ms: u64,
    pub total_ms: u64,
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
    pub error_scopes: usize,
    #[serde(default)]
    pub posting_hits: usize,
    #[serde(default)]
    pub hydrated_candidates: usize,
    #[serde(default)]
    pub scanned_candidates: usize,
    #[serde(default)]
    pub timings: RegexStageTimings,
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
    #[allow(dead_code)] // retained in the frozen candidate contract/golden fixtures
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

#[derive(Clone, Copy)]
struct CandidateLimits<'a> {
    cancel: Option<&'a std::sync::Arc<std::sync::atomic::AtomicBool>>,
    max_candidates: usize,
}

#[derive(Debug, Clone)]
struct RegexGraphScopeState {
    desired_epoch: u64,
    acknowledged_epoch: u64,
    candidate_count: usize,
    candidate_digest: String,
    tombstone: bool,
}

struct TrigramPrefilterPlan {
    matching_ready_uids: HashSet<String>,
    ready_scopes: HashSet<String>,
    dirty_scopes: HashSet<String>,
    error_scopes: HashSet<String>,
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

/// Extract the trigram condition any matching text must satisfy, expressed as
/// an OR-of-ANDs (DNF): the outer Vec is ORed, each inner set is ANDed.
///
/// This is the standard trigram-index construction (Russ Cox, "Regular
/// Expression Matching with a Trigram Index"):
///   trigrams("abcd") = "abc" AND "bcd"   -- conjuncts WITHIN one literal
///   match(e1|e2)     = match(e1) OR match(e2)  -- alternatives ACROSS branches
///
/// The literal extractor resolves alternations into alternative literals: for
/// `(alpha|beta)` it yields both, and a match needs only ONE of them — so each
/// literal becomes its own branch. Merging every literal's trigrams into a
/// single OR clause (nw-142) made a document match on any ONE shared trigram,
/// which selected ~40% of the corpus for a 20-character identifier. ANDing
/// ACROSS branches (the behavior before that) was equally wrong: it required
/// every alternation branch to appear in the same text and dropped real
/// matches. Per-branch conjunction, cross-branch disjunction is the shape that
/// is both correct and selective.
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

    // One branch per literal. Within a branch the trigrams are conjuncts; the
    // branches themselves are alternatives. A literal shorter than 3 chars
    // yields no trigrams → that branch cannot constrain the search, and since a
    // match may take that branch, the whole prefilter is unusable.
    let mut branches: Vec<HashSet<String>> = Vec::new();
    for lit in literals {
        // Inexact literals are prefixes/fragments; their trigrams are still a
        // necessary condition for the branch, so they remain usable.
        let lit_str = String::from_utf8_lossy(lit.as_bytes()).to_string();
        let tg = trigrams(&lit_str);
        if tg.is_empty() {
            // This alternation branch has no usable trigram → cannot prefilter.
            return None;
        }
        branches.push(tg);
    }
    if branches.is_empty() {
        return None;
    }
    Some(branches)
}

impl GraphStore {
    /// Incrementally refresh trigram postings over all indexed text. Existing
    /// callers keep the historical return value (postings written), while
    /// [`GraphStore::refresh_trigram_index`] exposes detailed work metrics.
    pub fn build_trigram_index(&self) -> Result<usize, StoreError> {
        Ok(self.refresh_trigram_index(false)?.postings_added)
    }

    /// Force a one-time full rebuild using the current regex-v3 schema.
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
        self.refresh_regex_v3(force_full)
    }

    fn refresh_regex_v3(&self, force_full: bool) -> Result<TrigramRefreshStats, StoreError> {
        let started = Instant::now();
        let root = self.regex_sidecar_root().ok_or_else(|| {
            StoreError::Query("regex v3 requires an on-disk graph store".to_string())
        })?;
        let identity = self.publication_identity()?.ok_or_else(|| {
            StoreError::Query("regex v3 requires graph publication identity".to_string())
        })?;
        let index = RegexIndex::with_reader_pool(root, self.regex_reader_pool.clone());
        match index.garbage_collect() {
            Ok(report) => {
                for failure in report.failures {
                    tracing::warn!(
                        scope_hash = %failure.scope_hash,
                        error = %failure.error,
                        "deferred regex shard garbage collection for one scope"
                    );
                }
            }
            Err(error) => tracing::warn!(%error, "deferred regex shard garbage collection"),
        }
        let active_scopes = self.active_regex_scopes()?;
        let metadata_report = match index.list_metadata() {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(%error, "regex shard metadata listing is unavailable");
                crate::regex_index::RegexMetadataReport {
                    metadata: Vec::new(),
                    failures: vec![crate::regex_index::RegexScopeIssue {
                        scope_hash: "<unknown>".to_string(),
                        error: error.to_string(),
                    }],
                }
            }
        };
        let unknown_metadata_failure = metadata_report
            .failures
            .iter()
            .any(|failure| failure.scope_hash == "<unknown>");
        for failure in &metadata_report.failures {
            tracing::warn!(
                scope_hash = %failure.scope_hash,
                error = %failure.error,
                "regex shard metadata is unavailable for one scope"
            );
        }
        let existing = metadata_report
            .metadata
            .into_iter()
            .map(|metadata| (metadata.scope_uid.clone(), metadata))
            .collect::<HashMap<_, _>>();
        // Metadata absence is itself scope-local evidence that acceleration is
        // unavailable, whether CURRENT is corrupt, missing, or was safely
        // retired. An unidentifiable directory-read failure is the sole case
        // that widens repair to every active scope.
        let unavailable_scopes = if unknown_metadata_failure {
            active_scopes.clone()
        } else {
            active_scopes
                .iter()
                .filter(|scope_uid| !existing.contains_key(scope_uid.as_str()))
                .cloned()
                .collect::<HashSet<_>>()
        };
        let mut stats = TrigramRefreshStats::default();
        let states = self.read_regex_scope_states()?;
        let mut work_scopes = if force_full {
            let mut scopes = active_scopes.clone();
            scopes.extend(states.keys().cloned());
            scopes.extend(existing.keys().cloned());
            scopes
        } else {
            self.regex_outbox_scopes()?
        };
        work_scopes.extend(unavailable_scopes);
        // A graph imported from a pre-v3 release has no outbox yet. Bootstrap
        // once; all subsequent normal work is driven only by coalesced scopes.
        if !force_full && work_scopes.is_empty() && (states.is_empty() || existing.is_empty()) {
            work_scopes = active_scopes.clone();
        }
        let mut ordered: Vec<_> = work_scopes.into_iter().collect();
        ordered.sort();
        for scope_uid in ordered {
            if !active_scopes.contains(&scope_uid) {
                let epoch = match states.get(&scope_uid) {
                    Some(state) if state.tombstone => state.desired_epoch,
                    _ => self.mark_regex_scope_dirty(&scope_uid, true)?,
                };
                if index.retire_scope(&scope_uid)? {
                    stats.scopes_refreshed += 1;
                }
                self.acknowledge_regex_tombstone(&scope_uid, epoch)?;
                continue;
            }

            let epoch = if force_full {
                self.mark_regex_scope_dirty(&scope_uid, false)?
            } else {
                match states.get(&scope_uid).filter(|state| !state.tombstone) {
                    Some(state) => state.desired_epoch,
                    None => self.mark_regex_scope_dirty(&scope_uid, false)?,
                }
            };
            let one_scope = HashSet::from([scope_uid.clone()]);
            let mut candidates = self
                .collect_candidates_for_scopes(
                    &one_scope,
                    None,
                    None,
                    Instant::now(),
                    u64::MAX,
                    CandidateLimits {
                        cancel: None,
                        max_candidates: usize::MAX,
                    },
                )?
                .0;
            candidates.sort_by(|left, right| left.uid.cmp(&right.uid));
            let digest = candidate_digest(&candidates);
            let prior = existing.get(&scope_uid);
            let compatible = prior.is_some_and(|metadata| {
                metadata.schema_version == REGEX_INDEX_SCHEMA_VERSION
                    && metadata.tokenizer_fingerprint == REGEX_TOKENIZER_FINGERPRINT
                    && metadata.brain_uuid == identity.brain_uuid
                    && metadata.publication_uuid == identity.publication_uuid
                    && metadata.candidate_count == candidates.len()
                    && metadata.candidate_digest == digest
                    && metadata.scope_epoch == epoch
            });
            let graph_acknowledged = states.get(&scope_uid).is_some_and(|state| {
                state.desired_epoch == epoch
                    && state.acknowledged_epoch == epoch
                    && !state.tombstone
                    && state.candidate_count == candidates.len()
                    && state.candidate_digest == digest
            });
            if compatible && graph_acknowledged && !force_full {
                stats.scopes_unchanged += 1;
                continue;
            }

            let prior_documents_result = index.document_hashes(&scope_uid);
            let prior_documents = prior_documents_result
                .as_ref()
                .ok()
                .and_then(|documents| documents.clone())
                .unwrap_or_default();
            let desired_documents: HashMap<_, _> = candidates
                .iter()
                .map(|candidate| (candidate.uid.clone(), candidate.text_hash.clone()))
                .collect();
            stats.nodes_added += desired_documents
                .keys()
                .filter(|uid| !prior_documents.contains_key(uid.as_str()))
                .count();
            stats.nodes_changed += desired_documents
                .iter()
                .filter(|(uid, hash)| {
                    prior_documents
                        .get(uid.as_str())
                        .is_some_and(|prior| prior != *hash)
                })
                .count();
            stats.nodes_deleted += prior_documents
                .keys()
                .filter(|uid| !desired_documents.contains_key(uid.as_str()))
                .count();
            let metadata = RegexShardMetadata {
                schema_version: REGEX_INDEX_SCHEMA_VERSION,
                tokenizer_fingerprint: REGEX_TOKENIZER_FINGERPRINT.to_string(),
                brain_uuid: identity.brain_uuid.clone(),
                publication_uuid: identity.publication_uuid.clone(),
                source_graph_generation: self.graph_generation(),
                scope_uid: scope_uid.clone(),
                scope_epoch: epoch,
                candidate_count: candidates.len(),
                candidate_digest: digest.clone(),
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
            let changed_uids: HashSet<_> = desired_documents
                .iter()
                .filter(|(uid, hash)| prior_documents.get(uid.as_str()) != Some(*hash))
                .map(|(uid, _)| uid.as_str())
                .collect();
            let deleted_uids: Vec<_> = prior_documents
                .keys()
                .filter(|uid| !desired_documents.contains_key(uid.as_str()))
                .cloned()
                .collect();
            let changed_documents: Vec<_> = documents
                .iter()
                .filter(|document| changed_uids.contains(document.uid))
                .cloned()
                .collect();
            let can_update = !force_full
                && prior_documents_result.is_ok()
                && prior.is_some_and(|prior| {
                    prior.schema_version == REGEX_INDEX_SCHEMA_VERSION
                        && prior.tokenizer_fingerprint == REGEX_TOKENIZER_FINGERPRINT
                        && prior.brain_uuid == identity.brain_uuid
                        && prior.publication_uuid == identity.publication_uuid
                });
            if can_update {
                index.update_scope(
                    prior.expect("checked above"),
                    metadata,
                    &changed_documents,
                    &deleted_uids,
                )?;
            } else {
                index.replace_scope(metadata, &documents)?;
            }
            self.acknowledge_regex_scope(&scope_uid, epoch, candidates.len(), &digest)?;

            stats.scopes_refreshed += 1;
            stats.postings_added += trigram_sets.iter().map(HashSet::len).sum::<usize>();
        }
        stats.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        match index.garbage_collect() {
            Ok(report) => {
                for failure in report.failures {
                    tracing::warn!(
                        scope_hash = %failure.scope_hash,
                        error = %failure.error,
                        "deferred regex shard garbage collection for one scope"
                    );
                }
            }
            Err(error) => tracing::warn!(%error, "deferred regex shard garbage collection"),
        }
        Ok(stats)
    }

    fn read_regex_scope_states(&self) -> Result<HashMap<String, RegexGraphScopeState>, StoreError> {
        let conn = self.conn()?;
        let rows = conn
            .query(
                "MATCH (s:RegexScopeState) RETURN s.uid, s.desired_epoch, \
                 s.acknowledged_epoch, s.candidate_count, s.candidate_digest, s.tombstone",
            )
            .map_err(|error| StoreError::Query(format!("read regex scope states: {error}")))?;
        let mut states = HashMap::new();
        for row in rows {
            if let [
                Value::String(uid),
                Value::Int64(desired),
                Value::Int64(acknowledged),
                Value::Int64(count),
                Value::String(digest),
                Value::Bool(tombstone),
            ] = row.as_slice()
            {
                states.insert(
                    uid.clone(),
                    RegexGraphScopeState {
                        desired_epoch: (*desired).max(0) as u64,
                        acknowledged_epoch: (*acknowledged).max(0) as u64,
                        candidate_count: (*count).max(0) as usize,
                        candidate_digest: digest.clone(),
                        tombstone: *tombstone,
                    },
                );
            }
        }
        Ok(states)
    }

    /// Number of scopes with coalesced regex work waiting to be reconciled.
    ///
    /// This is the cheap pre-check the daemon's reconcile loop runs on every
    /// tick so it only takes the write gate when there is something to do. A
    /// full [`Self::refresh_trigram_index`] pass also garbage-collects retired
    /// shard generations and repairs scopes whose metadata is missing, and
    /// neither of those is visible here — that work is deliberately picked up
    /// by the next pass that has outbox work anyway (an unreadable shard is
    /// already fail-open at query time, so deferring its repair costs latency,
    /// never correctness) plus one unconditional pass at daemon startup.
    pub fn pending_regex_scope_count(&self) -> Result<usize, StoreError> {
        Ok(self.regex_outbox_scopes()?.len())
    }

    fn regex_outbox_scopes(&self) -> Result<HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let rows = conn
            .query("MATCH (o:RegexScopeOutbox) RETURN o.uid")
            .map_err(|error| StoreError::Query(format!("read regex outbox: {error}")))?;
        Ok(rows
            .filter_map(|row| match row.first() {
                Some(Value::String(uid)) => Some(uid.clone()),
                _ => None,
            })
            .collect())
    }

    fn active_regex_scopes(&self) -> Result<HashSet<String>, StoreError> {
        let mut scopes: HashSet<String> = self
            .list_repos(None)?
            .into_iter()
            .map(|repo| repo.uid)
            .collect();
        scopes.extend(self.list_vaults(None)?.into_iter().map(|vault| vault.uid));
        // Corrupt/imported graphs and focused store tests can contain owned
        // candidates before their Repo/Vault root. Preserve correctness by
        // discovering ownership only when the authoritative root inventory is
        // empty; healthy production graphs take the constant-size root path.
        if scopes.is_empty() {
            let conn = self.conn()?;
            if let Ok(rows) = conn.query("MATCH (s:Symbol) RETURN s.repo_uid") {
                scopes.extend(rows.filter_map(|row| match row.first() {
                    Some(Value::String(uid)) if !uid.is_empty() => Some(uid.clone()),
                    _ => None,
                }));
            }
            if let Ok(rows) = conn.query("MATCH (n:Note) RETURN n.vault_uid") {
                scopes.extend(rows.filter_map(|row| match row.first() {
                    Some(Value::String(uid)) if !uid.is_empty() => Some(uid.clone()),
                    _ => None,
                }));
            }
        }
        Ok(scopes)
    }

    /// Collect all searchable candidate nodes (Sections, Notes, Frontmatter,
    /// Symbols), optionally filtered by `path_prefix` (matched against
    /// location) and `kinds` (case-insensitive kind names: "Section", "Note",
    /// "Frontmatter", "Symbol").
    ///
    /// "Frontmatter" is the fourth shape and was absent (nw-298). Frontmatter
    /// is split off the source before sectioning, so it reaches no Section, and
    /// the Note branch indexes only the title — so a string present ONLY in
    /// frontmatter was in the graph as a column and in no candidate at all.
    /// `brain_search` found the same string because its indexer reads the file
    /// off disk, so one database answered "yes" and "no" to the same question.
    fn collect_candidates(
        &self,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
        start: Instant,
        deadline_ms: u64,
        limits: CandidateLimits<'_>,
    ) -> Result<(Vec<Candidate>, Option<RegexTruncationReason>), StoreError> {
        let interrupted = || -> Result<bool, StoreError> {
            if limits
                .cancel
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(StoreError::Cancelled(CancelReason::Timeout));
            }
            Ok(elapsed_millis(start) >= deadline_ms)
        };
        let want_kind = |k: &str| -> bool {
            match kinds {
                None => true,
                Some(ks) => ks.iter().any(|want| want.eq_ignore_ascii_case(k)),
            }
        };

        let mut out = Vec::new();
        // Whether any whole-corpus scan below came back SHORT. The scans
        // tolerate a row they cannot decode rather than losing the corpus
        // (nw-335), so `Ok` no longer means "all of it" — and this function's
        // entire job is to report an incomplete candidate set as incomplete.
        let mut degraded = false;

        // Sections — body text is the richest source. We need the parent note's
        // file_path for the location, so build a note_uid -> path map once.
        if want_kind("Section") {
            let (notes, integrity) = self.list_notes_with_integrity(None)?;
            degraded |= integrity.is_degraded();
            if interrupted()? {
                return Ok((out, Some(RegexTruncationReason::Deadline)));
            }
            let note_path: HashMap<String, (String, String)> = notes
                .into_iter()
                .map(|n| (n.uid, (n.file_path, n.vault_uid)))
                .collect();
            let (sections, integrity) = self.list_all_sections_with_integrity()?;
            degraded |= integrity.is_degraded();
            for s in sections {
                if interrupted()? {
                    return Ok((out, Some(RegexTruncationReason::Deadline)));
                }
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
                if out.len() >= limits.max_candidates {
                    return Ok((out, Some(RegexTruncationReason::CandidateCap)));
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
            let (all_notes, integrity) = self.list_notes_with_integrity(None)?;
            degraded |= integrity.is_degraded();
            for n in all_notes {
                if interrupted()? {
                    return Ok((out, Some(RegexTruncationReason::Deadline)));
                }
                if n.title.is_empty() {
                    continue;
                }
                if let Some(prefix) = path_prefix
                    && !n.file_path.starts_with(prefix)
                {
                    continue;
                }
                if out.len() >= limits.max_candidates {
                    return Ok((out, Some(RegexTruncationReason::CandidateCap)));
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

        // Frontmatter — the raw YAML, with a real file line offset.
        if want_kind("Frontmatter") {
            let (all_notes, integrity) = self.list_notes_with_integrity(None)?;
            degraded |= integrity.is_degraded();
            for n in all_notes {
                if interrupted()? {
                    return Ok((out, Some(RegexTruncationReason::Deadline)));
                }
                let Some(raw) = n.frontmatter_raw.filter(|raw| !raw.is_empty()) else {
                    continue;
                };
                if let Some(prefix) = path_prefix
                    && !n.file_path.starts_with(prefix)
                {
                    continue;
                }
                if out.len() >= limits.max_candidates {
                    return Ok((out, Some(RegexTruncationReason::CandidateCap)));
                }
                out.push(Candidate {
                    // The note's own uid, deliberately: a synthetic
                    // `uid#frontmatter` would leak a string that names no node
                    // into `RegexMatch.uid`. Two candidates may share a uid with
                    // different kinds; the trigram loader yields both and the
                    // verification pass decides.
                    uid: n.uid,
                    scope_uid: n.vault_uid,
                    text_hash: text_hash(&raw),
                    kind: "Frontmatter".to_string(),
                    title: n.title,
                    location: format!("{}:{FRONTMATTER_START_LINE}", n.file_path),
                    text: raw,
                    // Line 1 is the opening `---`; the block's first line is 2.
                    start_line: FRONTMATTER_START_LINE,
                });
            }
        }

        // Symbols — signature text.
        if want_kind("Symbol") {
            let (symbols, integrity) = self.list_all_symbols_with_integrity()?;
            degraded |= integrity.is_degraded();
            for sym in symbols {
                if interrupted()? {
                    return Ok((out, Some(RegexTruncationReason::Deadline)));
                }
                if sym.signature.is_empty() {
                    continue;
                }
                if let Some(prefix) = path_prefix
                    && !sym.file_path.starts_with(prefix)
                {
                    continue;
                }
                let location = format!("{}:{}", sym.file_path, sym.start_line);
                if out.len() >= limits.max_candidates {
                    return Ok((out, Some(RegexTruncationReason::CandidateCap)));
                }
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

        Ok((
            out,
            interrupted()?
                .then_some(RegexTruncationReason::Deadline)
                // A budget stop outranks a short corpus (it is the more
                // specific reason the caller can act on), but a corpus that
                // lost rows must never fall through as `None`.
                .or_else(|| degraded.then_some(RegexTruncationReason::UndecodableRows)),
        ))
    }

    /// Collect fallback/rebuild text only for explicitly selected source
    /// scopes. Trusted regex shards never need a corpus-wide candidate load.
    fn collect_candidates_for_scopes(
        &self,
        scopes: &HashSet<String>,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
        start: Instant,
        deadline_ms: u64,
        limits: CandidateLimits<'_>,
    ) -> Result<(Vec<Candidate>, Option<RegexTruncationReason>), StoreError> {
        let interrupted = || -> Result<bool, StoreError> {
            if limits
                .cancel
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(StoreError::Cancelled(CancelReason::Timeout));
            }
            Ok(elapsed_millis(start) >= deadline_ms)
        };
        let want_kind = |kind: &str| {
            kinds.is_none_or(|values| values.iter().any(|value| value.eq_ignore_ascii_case(kind)))
        };
        let mut ordered: Vec<_> = scopes
            .iter()
            .filter(|uid| !uid.is_empty())
            .cloned()
            .collect();
        ordered.sort();
        let mut candidates = Vec::new();
        // See `collect_candidates`: a scan that came back short must not leave
        // the result claiming it was complete.
        let mut degraded = false;
        for scope_uid in ordered {
            if interrupted()? {
                return Ok((candidates, Some(RegexTruncationReason::Deadline)));
            }
            if want_kind("Symbol") {
                for symbol in self.lookup_symbols_by_repo(&scope_uid)? {
                    if interrupted()? {
                        return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                    }
                    if symbol.signature.is_empty()
                        || path_prefix.is_some_and(|prefix| !symbol.file_path.starts_with(prefix))
                    {
                        continue;
                    }
                    if candidates.len() >= limits.max_candidates {
                        return Ok((candidates, Some(RegexTruncationReason::CandidateCap)));
                    }
                    candidates.push(Candidate {
                        uid: symbol.uid,
                        scope_uid: symbol.repo_uid,
                        text_hash: text_hash(&symbol.signature),
                        kind: "Symbol".to_string(),
                        title: symbol.name,
                        location: format!("{}:{}", symbol.file_path, symbol.start_line),
                        text: symbol.signature,
                        start_line: symbol.start_line,
                    });
                }
            }
            let (notes, integrity) = self.list_notes_with_integrity(Some(&scope_uid))?;
            degraded |= integrity.is_degraded();
            if interrupted()? {
                return Ok((candidates, Some(RegexTruncationReason::Deadline)));
            }
            let note_paths: HashMap<_, _> = notes
                .iter()
                .map(|note| (note.uid.clone(), note.file_path.clone()))
                .collect();
            if want_kind("Note") {
                for note in &notes {
                    if interrupted()? {
                        return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                    }
                    if note.title.is_empty()
                        || path_prefix.is_some_and(|prefix| !note.file_path.starts_with(prefix))
                    {
                        continue;
                    }
                    if candidates.len() >= limits.max_candidates {
                        return Ok((candidates, Some(RegexTruncationReason::CandidateCap)));
                    }
                    candidates.push(Candidate {
                        uid: note.uid.clone(),
                        scope_uid: note.vault_uid.clone(),
                        text_hash: text_hash(&note.title),
                        kind: "Note".to_string(),
                        title: note.title.clone(),
                        location: note.file_path.clone(),
                        text: note.title.clone(),
                        start_line: 1,
                    });
                }
            }
            // nw-298: the same fourth shape the corpus-wide collector emits, so
            // the trusted-shard path and the full-scan path cannot disagree
            // about whether a frontmatter string exists.
            if want_kind("Frontmatter") {
                for note in &notes {
                    if interrupted()? {
                        return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                    }
                    let Some(raw) = note.frontmatter_raw.as_deref().filter(|r| !r.is_empty())
                    else {
                        continue;
                    };
                    if path_prefix.is_some_and(|prefix| !note.file_path.starts_with(prefix)) {
                        continue;
                    }
                    if candidates.len() >= limits.max_candidates {
                        return Ok((candidates, Some(RegexTruncationReason::CandidateCap)));
                    }
                    candidates.push(Candidate {
                        uid: note.uid.clone(),
                        scope_uid: note.vault_uid.clone(),
                        text_hash: text_hash(raw),
                        kind: "Frontmatter".to_string(),
                        title: note.title.clone(),
                        location: format!("{}:{FRONTMATTER_START_LINE}", note.file_path),
                        text: raw.to_string(),
                        start_line: FRONTMATTER_START_LINE,
                    });
                }
            }
            if want_kind("Section") {
                // nw-134: one bulk query per scope, not one per note.
                //
                // The previous loop called sections_in_note() for every note in
                // the scope. That filters on `note_uid`, which is NOT the primary
                // key and has no index, so lbug cannot rewrite it to a
                // PRIMARY_KEY_SCAN -- each call was a FULL Section-table scan. On a
                // ~1,050-note vault that is ~1,050 full scans where the pre-existing
                // collect_candidates() did exactly one.
                //
                // This is the path a dirty scope takes, so it is what both reported
                // "regex-search returns 0 results" cases were actually paying: a
                // watched vault edit dirties the vault scope, and collecting its
                // candidates consumed the whole time budget before any regex ran.
                //
                // NOT list_sections_by_vault: that traverses NOTE_HAS_SECTION, and
                // the edge is not guaranteed. write.rs:3022 deletes by the note_uid
                // PROPERTY precisely to catch "fragments whose NOTE_HAS_SECTION edge
                // is missing", so an edge traversal would silently drop those
                // sections. Ownership lives on the property; scan once and filter by
                // this scope's notes in memory.
                let (sections, integrity) = self.list_all_sections_with_integrity()?;
                degraded |= integrity.is_degraded();
                for section in sections {
                    if interrupted()? {
                        return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                    }
                    if !note_paths.contains_key(&section.note_uid) {
                        continue;
                    }
                    if section.text_content.is_empty() {
                        continue;
                    }
                    let path = note_paths
                        .get(&section.note_uid)
                        .cloned()
                        .unwrap_or_default();
                    if path_prefix.is_some_and(|prefix| !path.starts_with(prefix)) {
                        continue;
                    }
                    if candidates.len() >= limits.max_candidates {
                        return Ok((candidates, Some(RegexTruncationReason::CandidateCap)));
                    }
                    candidates.push(Candidate {
                        uid: section.uid,
                        scope_uid: scope_uid.clone(),
                        text_hash: if section.text_hash.is_empty() {
                            text_hash(&section.text_content)
                        } else {
                            section.text_hash
                        },
                        kind: "Section".to_string(),
                        title: String::new(),
                        location: format!("{path}:{}", section.start_line),
                        text: section.text_content,
                        start_line: section.start_line,
                    });
                }
            }
        }
        Ok((
            candidates,
            interrupted()?
                .then_some(RegexTruncationReason::Deadline)
                .or_else(|| degraded.then_some(RegexTruncationReason::UndecodableRows)),
        ))
    }

    /// Hydrate only derived-index hits, in bounded primary-key batches.
    fn load_candidates_by_uid(
        &self,
        uids: &HashSet<String>,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
        start: Instant,
        deadline_ms: u64,
        limits: CandidateLimits<'_>,
    ) -> Result<(Vec<Candidate>, Option<RegexTruncationReason>), StoreError> {
        let interrupted = || -> Result<bool, StoreError> {
            if limits
                .cancel
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(StoreError::Cancelled(CancelReason::Timeout));
            }
            Ok(elapsed_millis(start) >= deadline_ms)
        };
        let want_kind = |kind: &str| {
            kinds.is_none_or(|values| values.iter().any(|value| value.eq_ignore_ascii_case(kind)))
        };
        let mut ordered: Vec<_> = uids.iter().cloned().collect();
        ordered.sort();
        let mut candidates = Vec::new();
        if want_kind("Symbol") {
            for chunk in ordered.chunks(256) {
                if interrupted()? {
                    return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                }
                for symbol in self.lookup_symbols_by_uids(chunk)? {
                    if symbol.signature.is_empty()
                        || path_prefix.is_some_and(|prefix| !symbol.file_path.starts_with(prefix))
                    {
                        continue;
                    }
                    if candidates.len() >= limits.max_candidates {
                        return Ok((candidates, Some(RegexTruncationReason::CandidateCap)));
                    }
                    candidates.push(Candidate {
                        uid: symbol.uid,
                        scope_uid: symbol.repo_uid,
                        text_hash: text_hash(&symbol.signature),
                        kind: "Symbol".to_string(),
                        title: symbol.name,
                        location: format!("{}:{}", symbol.file_path, symbol.start_line),
                        text: symbol.signature,
                        start_line: symbol.start_line,
                    });
                }
            }
        }
        // Notes and Frontmatter share a uid, so one lookup serves both.
        if want_kind("Note") || want_kind("Frontmatter") {
            for chunk in ordered.chunks(256) {
                if interrupted()? {
                    return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                }
                for note in self.lookup_notes_by_uids(chunk)? {
                    if path_prefix.is_some_and(|prefix| !note.file_path.starts_with(prefix)) {
                        continue;
                    }
                    if candidates.len() >= limits.max_candidates {
                        return Ok((candidates, Some(RegexTruncationReason::CandidateCap)));
                    }
                    // nw-298: a posting for this uid may have come from the
                    // note's TITLE or from its FRONTMATTER — the two share a
                    // uid — so emit both shapes and let verification decide.
                    if let Some(raw) = note.frontmatter_raw.as_deref().filter(|r| !r.is_empty())
                        && want_kind("Frontmatter")
                    {
                        candidates.push(Candidate {
                            uid: note.uid.clone(),
                            scope_uid: note.vault_uid.clone(),
                            text_hash: text_hash(raw),
                            kind: "Frontmatter".to_string(),
                            title: note.title.clone(),
                            location: format!("{}:{FRONTMATTER_START_LINE}", note.file_path),
                            text: raw.to_string(),
                            start_line: FRONTMATTER_START_LINE,
                        });
                    }
                    if note.title.is_empty() || !want_kind("Note") {
                        continue;
                    }
                    candidates.push(Candidate {
                        uid: note.uid,
                        scope_uid: note.vault_uid,
                        text_hash: text_hash(&note.title),
                        kind: "Note".to_string(),
                        title: note.title.clone(),
                        location: note.file_path,
                        text: note.title,
                        start_line: 1,
                    });
                }
            }
        }
        if want_kind("Section") {
            let mut sections = Vec::new();
            for chunk in ordered.chunks(256) {
                if interrupted()? {
                    return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                }
                sections.extend(self.lookup_sections_by_uids(chunk)?);
            }
            let note_uids: Vec<_> = sections
                .iter()
                .map(|section| section.note_uid.clone())
                .collect();
            let mut note_paths = HashMap::new();
            for chunk in note_uids.chunks(256) {
                if interrupted()? {
                    return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                }
                note_paths.extend(
                    self.lookup_notes_by_uids(chunk)?
                        .into_iter()
                        .map(|note| (note.uid, (note.file_path, note.vault_uid))),
                );
            }
            for section in sections {
                if interrupted()? {
                    return Ok((candidates, Some(RegexTruncationReason::Deadline)));
                }
                if section.text_content.is_empty() {
                    continue;
                }
                let (path, scope_uid) = note_paths
                    .get(&section.note_uid)
                    .cloned()
                    .unwrap_or_default();
                if path_prefix.is_some_and(|prefix| !path.starts_with(prefix)) {
                    continue;
                }
                if candidates.len() >= limits.max_candidates {
                    return Ok((candidates, Some(RegexTruncationReason::CandidateCap)));
                }
                candidates.push(Candidate {
                    uid: section.uid,
                    scope_uid,
                    text_hash: if section.text_hash.is_empty() {
                        text_hash(&section.text_content)
                    } else {
                        section.text_hash
                    },
                    kind: "Section".to_string(),
                    title: String::new(),
                    location: format!("{path}:{}", section.start_line),
                    text: section.text_content,
                    start_line: section.start_line,
                });
            }
        }
        Ok((
            candidates,
            interrupted()?.then_some(RegexTruncationReason::Deadline),
        ))
    }

    fn regex_v3_candidate_uids(
        &self,
        clauses: &[HashSet<String>],
        start: Instant,
        deadline_ms: u64,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Option<TrigramPrefilterPlan>, StoreError> {
        let interrupted = || -> Result<bool, StoreError> {
            if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Err(StoreError::Cancelled(CancelReason::Timeout));
            }
            Ok(elapsed_millis(start) >= deadline_ms)
        };
        if interrupted()? {
            return Ok(None);
        }
        let Some(root) = self.regex_sidecar_root() else {
            let dirty_scopes = self.active_regex_scopes()?;
            if interrupted()? {
                return Ok(None);
            }
            return Ok(Some(TrigramPrefilterPlan {
                matching_ready_uids: HashSet::new(),
                ready_scopes: HashSet::new(),
                dirty_scopes,
                error_scopes: HashSet::new(),
                has_index: false,
            }));
        };
        let index = RegexIndex::with_reader_pool(root, self.regex_reader_pool.clone());
        let identity = self.publication_identity()?.ok_or_else(|| {
            StoreError::Query("regex v3 requires graph publication identity".to_string())
        })?;
        let states = self.read_regex_scope_states()?;
        let active_scopes = self.active_regex_scopes()?;
        let has_index = index.root().join("scopes").is_dir();
        let mut ready_scopes = HashSet::new();
        let mut dirty_scopes = HashSet::new();
        let mut error_scopes = HashSet::new();
        let mut matching_ready_uids = HashSet::new();
        for scope_uid in active_scopes {
            if interrupted()? {
                return Ok(None);
            }
            let Some(state) = states.get(&scope_uid) else {
                dirty_scopes.insert(scope_uid);
                continue;
            };
            if state.tombstone || state.desired_epoch != state.acknowledged_epoch {
                dirty_scopes.insert(scope_uid);
                continue;
            }
            let metadata = match index.metadata(&scope_uid) {
                Ok(Some(metadata)) => metadata,
                Ok(None) => {
                    dirty_scopes.insert(scope_uid);
                    continue;
                }
                Err(_) => {
                    error_scopes.insert(scope_uid.clone());
                    dirty_scopes.insert(scope_uid);
                    continue;
                }
            };
            let trusted = metadata.schema_version == REGEX_INDEX_SCHEMA_VERSION
                && metadata.tokenizer_fingerprint == REGEX_TOKENIZER_FINGERPRINT
                && metadata.brain_uuid == identity.brain_uuid
                && metadata.publication_uuid == identity.publication_uuid
                && metadata.source_graph_generation <= self.graph_generation()
                && metadata.scope_epoch == state.acknowledged_epoch
                && metadata.candidate_count == state.candidate_count
                && metadata.candidate_digest == state.candidate_digest;
            if !trusted {
                dirty_scopes.insert(scope_uid);
                continue;
            }
            match index.candidate_uids(&metadata, clauses, CANDIDATE_CAP) {
                Ok(Some(uids)) => {
                    ready_scopes.insert(scope_uid);
                    matching_ready_uids.extend(uids);
                }
                Ok(None) => {
                    dirty_scopes.insert(scope_uid);
                }
                Err(_) => {
                    error_scopes.insert(scope_uid.clone());
                    dirty_scopes.insert(scope_uid);
                }
            }
        }

        if has_index
            && !dirty_scopes.is_empty()
            && !TRIGRAM_STALE_WARNED.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "warning: {} regex shard(s) are unavailable or stale; scanning only those scopes — rerun \
                 `index --with-trigrams` to repair them, or set `[indexing] with_trigrams = true` \
                 so indexing keeps them fresh",
                dirty_scopes.len()
            );
        } else if dirty_scopes.is_empty() {
            TRIGRAM_STALE_WARNED.store(false, Ordering::Relaxed);
        }
        // A single shard lookup may consume the remaining budget. Re-check
        // before handing a seemingly usable plan to hydration; otherwise a
        // one-scope corpus can overrun during planning and still launch the
        // most expensive phase.
        if interrupted()? {
            return Ok(None);
        }
        Ok(Some(TrigramPrefilterPlan {
            matching_ready_uids,
            ready_scopes,
            dirty_scopes,
            error_scopes,
            has_index,
        }))
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
        self.regex_search_cancellable(pattern, path_prefix, kinds, limit, max_millis, None)
    }

    /// Regex search with cooperative cancellation across every phase.
    /// Internal `max_millis` exhaustion returns an honestly truncated result;
    /// an RPC/client cancellation returns `StoreError::Cancelled` so it cannot
    /// be cached or mistaken for a complete empty answer.
    pub fn regex_search_cancellable(
        &self,
        pattern: &str,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
        limit: Option<usize>,
        max_millis: Option<u64>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<RegexSearchResult, StoreError> {
        self.regex_search_cancellable_with_candidate_cap(
            pattern,
            path_prefix,
            kinds,
            limit,
            max_millis,
            cancel,
            CANDIDATE_CAP,
        )
    }

    /// `regex_search_cancellable` with the candidate cap parameterized
    /// instead of hardcoded to [`CANDIDATE_CAP`]. The public entry point above
    /// always passes the real constant; this seam exists so a unit test can
    /// force `hydration_stop = Some(CandidateCap)` on a small, fast in-memory
    /// store (nw-427) rather than needing 200,000 real rows to hit the
    /// production cap — `CandidateCap` and `Deadline` both flow through the
    /// identical `hydration_stop` -> verification-loop path this item fixes,
    /// so exercising one deterministically exercises the other.
    fn regex_search_cancellable_with_candidate_cap(
        &self,
        pattern: &str,
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
        limit: Option<usize>,
        max_millis: Option<u64>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
        candidate_cap: usize,
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
        let check_cancel = || {
            if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                Err(StoreError::Cancelled(CancelReason::Timeout))
            } else {
                Ok(())
            }
        };
        check_cancel()?;

        let planning_started = Instant::now();
        let clauses = required_trigram_clauses(pattern);
        let (plan, mut planning_deadline) = match &clauses {
            Some(clauses) => {
                match self.regex_v3_candidate_uids(clauses, start, deadline_ms, cancel)? {
                    Some(plan) => (Some(plan), false),
                    None => (None, true),
                }
            }
            None => (None, elapsed_millis(start) >= deadline_ms),
        };
        planning_deadline |=
            elapsed_millis(start).saturating_add(PHASE_ADMISSION_MILLIS) >= deadline_ms;
        let planning_ms = elapsed_millis(planning_started);
        let scanned_fallback = plan
            .as_ref()
            .is_none_or(|plan| !plan.dirty_scopes.is_empty());
        let stale_index = plan
            .as_ref()
            .is_some_and(|plan| plan.has_index && !plan.dirty_scopes.is_empty());
        let ready_scopes = plan.as_ref().map_or(0, |plan| plan.ready_scopes.len());
        let dirty_scopes = plan.as_ref().map_or(0, |plan| plan.dirty_scopes.len());
        let error_scopes = plan.as_ref().map_or(0, |plan| plan.error_scopes.len());
        let posting_hits = plan
            .as_ref()
            .map_or(0, |plan| plan.matching_ready_uids.len());

        check_cancel()?;
        let hydration_started = Instant::now();
        let mut hydration_stop = None;
        let (mut candidates, hydrated_candidates) = if planning_deadline {
            (Vec::new(), 0)
        } else {
            match &plan {
                Some(plan) => {
                    let (mut candidates, fallback_stop) = self.collect_candidates_for_scopes(
                        &plan.dirty_scopes,
                        path_prefix,
                        kinds,
                        start,
                        deadline_ms,
                        CandidateLimits {
                            cancel,
                            max_candidates: candidate_cap,
                        },
                    )?;
                    let (mut hydrated, hydrated_stop) = self.load_candidates_by_uid(
                        &plan.matching_ready_uids,
                        path_prefix,
                        kinds,
                        start,
                        deadline_ms,
                        CandidateLimits {
                            cancel,
                            max_candidates: candidate_cap,
                        },
                    )?;
                    hydration_stop = stronger_truncation(fallback_stop, hydrated_stop);
                    let remaining = candidate_cap.saturating_sub(candidates.len());
                    if hydrated.len() > remaining {
                        hydrated.truncate(remaining);
                        hydration_stop = Some(RegexTruncationReason::CandidateCap);
                    }
                    let hydrated_candidates = hydrated.len();
                    candidates.extend(hydrated);
                    (candidates, hydrated_candidates)
                }
                None => {
                    let (candidates, stop) = self.collect_candidates(
                        path_prefix,
                        kinds,
                        start,
                        deadline_ms,
                        CandidateLimits {
                            cancel,
                            max_candidates: candidate_cap,
                        },
                    )?;
                    hydration_stop = stop;
                    (candidates, 0)
                }
            }
        };
        check_cancel()?;
        candidates.sort_by(|left, right| left.uid.cmp(&right.uid));
        let hydration_ms = elapsed_millis(hydration_started);

        // Scan the full candidate set collection managed to gather, bounded by
        // a phase-local wall-clock budget (and a high safety ceiling) — NOT a
        // low pre-truncation. `truncated` is set ONLY when the scan actually
        // stops early or the corpus wasn't fully gathered, so `truncated:true`
        // with an empty `results` now genuinely means "incomplete" rather than
        // "the match was ordered past a 5000 cap and never scanned" (nw-076).
        // A short CORPUS is not a stopped SCAN, and the two must not share a
        // channel. `hydration_stop` feeds `truncated`, and `truncated` breaks
        // the verification loop below on its first iteration — so folding
        // "some rows were undecodable" in here would make ONE unreadable row
        // return zero results while reporting `truncated: true`, which is the
        // nw-076 dishonesty in its worst form. Carry it alongside instead:
        // scan everything that COULD be read, then say the corpus was short.
        let corpus_degraded = hydration_stop == Some(RegexTruncationReason::UndecodableRows);
        if corpus_degraded {
            hydration_stop = None;
        }
        let elapsed_deadline = elapsed_millis(start) >= deadline_ms;
        // nw-427: collection possibly not having gathered the FULL corpus
        // (`hydration_stop`/`elapsed_deadline`) is a fact about COMPLETENESS,
        // not a reason to discard the candidates it already gathered —
        // verifying an in-memory candidate is orders of magnitude cheaper than
        // the graph lookups that produced it, and this crate's regex engine is
        // finite-automata/linear-time (see `compile_pattern`), so scanning a
        // `candidate_cap`-bounded, already-collected set cannot itself blow
        // up. Only `planning_deadline` gates the loop's entry below, because
        // it is the one case where `candidates` is guaranteed empty (`if
        // planning_deadline { (Vec::new(), 0) }` above) — there is nothing to
        // lose by skipping it. Whether collection was complete is folded back
        // in AFTER the loop runs, not before.
        let collection_incomplete = hydration_stop.is_some() || elapsed_deadline;
        let mut truncated = planning_deadline;
        let mut truncation_reason = planning_deadline.then_some(RegexTruncationReason::Deadline);
        let mut results = Vec::new();
        let mut scanned_candidates = 0usize;
        // Verification's own ceiling is the budget REMAINING after collection,
        // not a fresh `deadline_ms` -- see `verification_budget_ms` and the
        // invariant note on the check below. Computed once, right before the
        // loop starts, so it reflects how much of the caller's stated budget
        // collection actually spent.
        let verification_budget_ms = verification_budget_ms(deadline_ms, elapsed_millis(start));
        let verification_started = Instant::now();
        for (i, c) in candidates.iter().enumerate() {
            check_cancel()?;
            if truncated {
                break;
            }
            // INVARIANT: total wall time is bounded by ~`deadline_ms` overall
            // (plus this loop's own single-iteration slack), never ~2x it.
            // Verification still gets a phase-local ceiling, measured from
            // `verification_started` rather than the search's overall
            // `start`, so a pathologically large already-collected set can't
            // itself run unbounded -- but that ceiling is
            // `verification_budget_ms`, the budget REMAINING after collection
            // (`deadline_ms - elapsed(start)`), not a second full
            // `deadline_ms` measured from a fresh clock. Reusing the full
            // budget here is exactly what let verification silently tack up
            // to another whole `deadline_ms` on top of collection: total wall
            // time could reach ~2x `--max-millis`, and on the common,
            // non-truncated path (where `DEFAULT_MAX_MILLIS` is the effective
            // budget) that showed up as a ~5x slowdown.
            //
            // When collection alone already consumed the full budget,
            // `verification_budget_ms` saturates to 0 -- but the check below
            // still lets the FIRST iteration through (elapsed-so-far is
            // ~0ms, which is not `> 0`), so whatever collection already
            // gathered still gets a chance to be verified rather than
            // discarded outright. That is nw-427's correctness win, kept: an
            // already-exhausted budget yields "verify what little time
            // allows, then truncate honestly," never "discard everything"
            // (the pre-nw-427 bug) and never "keep scanning past the
            // caller's stated budget" (this regression).
            if verification_started.elapsed().as_millis() as u64 > verification_budget_ms {
                truncated = true;
                truncation_reason = Some(RegexTruncationReason::Deadline);
                break;
            }
            if i >= candidate_cap {
                truncated = true;
                truncation_reason = Some(RegexTruncationReason::CandidateCap);
                break;
            }
            scanned_candidates += 1;
            // nw-300, the wider half: this walked `re.find`, the FIRST match
            // only, so exactly one result was emitted per candidate NODE and
            // `line`/`snippet` described that first occurrence alone. 449
            // occurrences in one file came back as 2 results with
            // `truncated: false` — a silent collapse that, unlike
            // `count_patterns`, was disclosed on NEITHER the MCP nor the CLI
            // surface.
            //
            // `find_iter` is the same non-overlapping leftmost-first walk
            // `count_patterns` uses, so the two exact-match surfaces now agree
            // about how many matches exist. The limit is still enforced, and
            // `truncated` now accounts for occurrences left unemitted WITHIN a
            // node as well as candidates left unscanned after it.
            let mut matched_any = false;
            let mut node_exhausted = true;
            for m in re.find_iter(&c.text) {
                matched_any = true;
                // Check the limit BEFORE pushing: with `--limit 0` the caller
                // asked for no results, so even the first match must not be
                // returned (previously one result slipped through).
                if results.len() >= limit {
                    node_exhausted = false;
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
            }
            if matched_any && results.len() >= limit {
                truncated = !node_exhausted || i + 1 < candidates.len();
                if truncated {
                    truncation_reason = Some(RegexTruncationReason::ResultLimit);
                }
                break;
            }
        }
        let verification_ms = elapsed_millis(verification_started);

        // The verification loop ran over everything COLLECTION handed it
        // without itself hitting a limit — but collection may not have
        // gathered the full corpus (`collection_incomplete`, computed before
        // the loop from `hydration_stop`/`elapsed_deadline`). Surface that now
        // rather than before the loop ran: nothing collected was thrown away
        // (that was the bug), but "no more matches exist" still is not
        // established when the corpus itself was cut short.
        if !truncated && collection_incomplete {
            truncated = true;
            // Preserve the original priority: an overall-elapsed deadline is
            // the more actionable reason to surface (the caller can raise
            // `--max-millis`), even when `hydration_stop` independently named
            // a different cause such as `CandidateCap`.
            truncation_reason = Some(if elapsed_deadline {
                RegexTruncationReason::Deadline
            } else {
                hydration_stop.unwrap_or(RegexTruncationReason::Deadline)
            });
        }

        // The scan ran to completion over a corpus that was missing rows. The
        // results are real, but "no more matches exist" is not established —
        // so the result declares itself partial and names why. A stop that
        // already happened is the more actionable reason and keeps its place.
        if corpus_degraded && !truncated {
            truncated = true;
            truncation_reason = Some(RegexTruncationReason::UndecodableRows);
        }

        // nw-097: attach the note at the source so no caller has to remember.
        Ok(RegexSearchResult {
            results,
            truncated,
            scanned_fallback,
            stale_index,
            ready_scopes,
            dirty_scopes,
            error_scopes,
            posting_hits,
            hydrated_candidates,
            scanned_candidates,
            truncation_reason,
            timings: RegexStageTimings {
                planning_ms,
                hydration_ms,
                verification_ms,
                total_ms: elapsed_millis(start),
            },
            note: None,
        }
        .with_scan_budget_note())
    }

    /// Counts-only companion to `regex_search`. For each pattern, returns the
    /// number of OCCURRENCES (non-overlapping, leftmost-first — exactly what
    /// `grep -o | wc -l` counts), the number of distinct files that matched,
    /// and the top files by occurrence count. Reuses the same trigram
    /// pre-filter and full-scan fallback.
    ///
    /// It used to count one per matching NODE and report that as
    /// `total_matches` (nw-300).
    pub fn count_patterns(
        &self,
        patterns: &[String],
        path_prefix: Option<&str>,
        kinds: Option<&[String]>,
    ) -> Result<Vec<PatternCount>, StoreError> {
        let mut out = Vec::new();
        for pattern in patterns {
            let started = Instant::now();
            let re = compile_pattern(pattern)?;

            // Optional trigram narrowing.
            let planning_started = Instant::now();
            let clauses = required_trigram_clauses(pattern);
            let plan = match &clauses {
                Some(clauses) => self.regex_v3_candidate_uids(clauses, started, u64::MAX, None)?,
                None => None,
            };
            let planning_ms = elapsed_millis(planning_started);
            let stale_index = plan
                .as_ref()
                .is_some_and(|plan| plan.has_index && !plan.dirty_scopes.is_empty());

            let mut per_file: HashMap<String, u64> = HashMap::new();
            let mut total: u64 = 0;
            let mut scanned_candidates = 0usize;
            let hydration_started = Instant::now();
            let (candidates, hydrated_candidates) = match &plan {
                Some(plan) => {
                    let mut candidates = self
                        .collect_candidates_for_scopes(
                            &plan.dirty_scopes,
                            path_prefix,
                            kinds,
                            started,
                            u64::MAX,
                            CandidateLimits {
                                cancel: None,
                                max_candidates: usize::MAX,
                            },
                        )?
                        .0;
                    let hydrated = self
                        .load_candidates_by_uid(
                            &plan.matching_ready_uids,
                            path_prefix,
                            kinds,
                            started,
                            u64::MAX,
                            CandidateLimits {
                                cancel: None,
                                max_candidates: usize::MAX,
                            },
                        )?
                        .0;
                    let hydrated_candidates = hydrated.len();
                    candidates.extend(hydrated);
                    (candidates, hydrated_candidates)
                }
                None => (
                    self.collect_candidates(
                        path_prefix,
                        kinds,
                        started,
                        u64::MAX,
                        CandidateLimits {
                            cancel: None,
                            max_candidates: usize::MAX,
                        },
                    )?
                    .0,
                    0,
                ),
            };
            let hydration_ms = elapsed_millis(hydration_started);
            let verification_started = Instant::now();
            for c in &candidates {
                scanned_candidates += 1;
                // nw-300: `is_match` is a boolean, so `total` counted NODES
                // that contained at least one match and reported that under a
                // field named `total_matches`. 449 occurrences of a word inside
                // two sections came back as 2, next to `truncated: false`.
                //
                // `find_iter` is non-overlapping leftmost-first, which is what
                // `grep -o` counts — so the value now equals the ground truth a
                // caller would verify against. Cost is one extra scan of
                // already-hot text, on matching candidates only.
                let occurrences = re.find_iter(&c.text).count() as u64;
                if occurrences > 0 {
                    total += occurrences;
                    let file = file_of(&c.location);
                    *per_file.entry(file).or_insert(0) += occurrences;
                }
            }
            let verification_ms = elapsed_millis(verification_started);

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
                error_scopes: plan.as_ref().map_or(0, |plan| plan.error_scopes.len()),
                posting_hits: plan
                    .as_ref()
                    .map_or(0, |plan| plan.matching_ready_uids.len()),
                hydrated_candidates,
                scanned_candidates,
                timings: RegexStageTimings {
                    planning_ms,
                    hydration_ms,
                    verification_ms,
                    total_ms: elapsed_millis(started),
                },
            });
        }
        Ok(out)
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// The verification phase's own wall-clock ceiling: the caller's `deadline_ms`
/// budget MINUS whatever collection already spent of it — never a second full
/// `deadline_ms` measured from a fresh clock. That reuse is exactly nw-427's
/// regression: verification could silently add up to another whole
/// `deadline_ms` on top of collection, so total wall time reached ~2x
/// `--max-millis` (and ~5x on the common, non-truncated default-budget path).
///
/// Saturates to 0 rather than underflowing when collection already consumed
/// the entire budget (or overran it) — `elapsed_since_start_ms > deadline_ms`
/// is the routine, expected case per nw-427's own measurements, not an error.
/// A `verification_budget_ms` of 0 does not mean "verify nothing": the loop's
/// own ceiling check compares elapsed time *within* the loop against this
/// value, and that starts at 0, so the first already-collected candidate is
/// still verified before the check can ever trip. That is nw-427's
/// correctness win, kept: an exhausted budget still returns real,
/// already-collected results (honestly marked truncated), rather than
/// discarding them the way the pre-nw-427 code did.
fn verification_budget_ms(deadline_ms: u64, elapsed_since_start_ms: u64) -> u64 {
    deadline_ms.saturating_sub(elapsed_since_start_ms)
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
    use std::sync::Arc;

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
            error_scopes: 0,
            posting_hits: 0,
            hydrated_candidates: 0,
            scanned_candidates: 0,
            truncation_reason: Some(RegexTruncationReason::Unknown),
            timings: RegexStageTimings::default(),
            note: None,
        }
        .with_scan_budget_note();
        assert_eq!(r.note.as_deref(), Some(SCAN_BUDGET_NOTE));
    }

    #[test]
    fn regex_deadline_stops_before_candidate_hydration() {
        let store = GraphStore::in_memory().unwrap();
        populate_store(&store);
        let result = store
            .regex_search("authenticateUser", None, None, None, Some(0))
            .unwrap();
        assert!(result.truncated);
        assert_eq!(
            result.truncation_reason,
            Some(RegexTruncationReason::Deadline)
        );
        assert_eq!(result.hydrated_candidates, 0);
        assert_eq!(result.scanned_candidates, 0);
    }

    /// The verification phase's ceiling must be the budget REMAINING after
    /// collection, never a second full `deadline_ms` measured from a fresh
    /// clock -- that reuse is the post-nw-427 regression this item fixes
    /// (total wall time reaching ~2x `--max-millis`, ~5x on the common
    /// default-budget path). Pure arithmetic, no wall clock involved, so this
    /// is exact and never flaky: the fixture is plain integers, chosen to
    /// cover the three cases that matter (partial spend, exact/over spend,
    /// minimal spend) rather than exercising the fix through real timing.
    ///
    /// Counterweight (verified by hand, not committed): reverting the
    /// production code to the pre-fix shape --
    /// `fn verification_budget_ms(deadline_ms: u64, _elapsed: u64) -> u64 { deadline_ms }`
    /// -- makes the first two assertions below fail (`400` instead of `50`,
    /// and `400` instead of `0`), which is exactly the bug: verification
    /// handed the full budget again regardless of what collection already
    /// spent.
    #[test]
    fn verification_budget_is_remaining_not_a_fresh_full_deadline() {
        // Collection spent 350 of a 400ms budget: 50ms should remain for
        // verification, not another full 400ms.
        assert_eq!(verification_budget_ms(400, 350), 50);
        // Collection spent the ENTIRE budget: remaining saturates to 0 rather
        // than continuing to hand out the full deadline.
        assert_eq!(verification_budget_ms(400, 400), 0);
        // Collection OVERRAN the budget (the routine case per nw-427's own
        // measurements -- collection is allowed to run right up to or past
        // `deadline_ms`): remaining still saturates to 0, not an underflowed
        // wraparound to a huge u64.
        assert_eq!(verification_budget_ms(400, 999), 0);
        // Collection was fast: almost the entire deadline remains.
        assert_eq!(verification_budget_ms(400, 1), 399);
        // No budget was ever requested and none was spent: still zero, not a
        // no-op that accidentally lets `0.saturating_sub(0)` mean "unbounded".
        assert_eq!(verification_budget_ms(0, 0), 0);
    }

    /// nw-427: when collection stops early (`hydration_stop.is_some()`) it
    /// must not discard the candidates it already gathered. Forces
    /// `hydration_stop = Some(CandidateCap)` deterministically (no reliance
    /// on wall-clock timing, which would make this test flaky) by giving
    /// `regex_search_cancellable_with_candidate_cap` a cap smaller than the
    /// number of matching symbols in one repo scope. Before the fix,
    /// `truncated` was seeded from `hydration_stop.is_some()` BEFORE the
    /// verification loop ran, so the loop broke on its very first iteration
    /// and `scanned_candidates`/`results` stayed at zero even though
    /// `candidates` held real, already-collected matches.
    #[test]
    fn hydration_candidate_cap_does_not_discard_already_collected_candidates() {
        let store = GraphStore::in_memory().unwrap();
        for i in 0..4 {
            store
                .insert_symbol(&Symbol {
                    uid: format!("sym:cap:{i}"),
                    name: "authenticateUser".to_string(),
                    kind: SymbolKind::Function,
                    repo_uid: "repo:cap".to_string(),
                    file_path: format!("src/auth_{i}.rs"),
                    start_line: 1,
                    end_line: 2,
                    signature: "fn authenticateUser(req: Request) -> Result<Token>".to_string(),
                    summary: None,
                    content_hash: format!("c{i}"),
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

        // Four matching symbols exist; cap collection at 2 so
        // `collect_candidates_for_scopes` returns exactly 2 candidates with
        // `Some(RegexTruncationReason::CandidateCap)` — a real, non-empty
        // `candidates` Vec paired with a non-`None` `hydration_stop`, which is
        // precisely the shape the discard bug required.
        let result = store
            .regex_search_cancellable_with_candidate_cap(
                "authenticateUser",
                None,
                None,
                None,
                None,
                None,
                2,
            )
            .unwrap();

        assert!(result.truncated, "collection was capped, so this is honest");
        assert_eq!(
            result.truncation_reason,
            Some(RegexTruncationReason::CandidateCap),
            "{result:?}"
        );
        assert_eq!(
            result.scanned_candidates, 2,
            "the 2 candidates collection DID gather must be verified, not discarded: {result:?}"
        );
        assert_eq!(
            result.results.len(),
            2,
            "both collected candidates actually match the pattern: {result:?}"
        );
        assert!(
            result.note.is_none(),
            "results are non-empty, so no scan-budget hedge note applies: {result:?}"
        );
    }

    #[test]
    fn regex_external_cancellation_is_an_error_not_an_empty_result() {
        let store = GraphStore::in_memory().unwrap();
        populate_store(&store);
        let cancel = Arc::new(AtomicBool::new(true));
        let error = store
            .regex_search_cancellable("authenticateUser", None, None, None, None, Some(&cancel))
            .unwrap_err();
        assert!(error.is_cancelled(), "{error}");
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
            error_scopes: 0,
            posting_hits: 0,
            hydrated_candidates: 0,
            scanned_candidates: 0,
            truncation_reason: None,
            timings: RegexStageTimings::default(),
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
            error_scopes: 0,
            posting_hits: 0,
            hydrated_candidates: 0,
            scanned_candidates: 0,
            truncation_reason: Some(RegexTruncationReason::Unknown),
            timings: RegexStageTimings::default(),
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

    /// nw-142: within ONE literal the trigrams are CONJUNCTS - a match must
    /// contain all of them. Only across alternation branches are they
    /// alternatives. Unioning everything into one OR clause makes the
    /// prefilter select any document sharing a single common trigram.
    ///
    /// Reference: Russ Cox, "Regular Expression Matching with a Trigram Index":
    ///   trigrams("abcd") = "abc" AND "bcd"
    ///   match(e1|e2)     = match(e1) OR match(e2)
    #[test]
    fn a_single_literal_yields_one_conjunctive_branch() {
        let branches = required_trigram_clauses("rollback_current").expect("usable literal");
        assert_eq!(
            branches.len(),
            1,
            "a plain string is ONE alternation branch, got {branches:?}"
        );
        // "rollback_current" is 16 chars -> 14 distinct trigrams, all required.
        assert_eq!(branches[0], trigrams("rollback_current"));
        assert!(
            branches[0].len() > 5,
            "a 16-char literal must contribute many required trigrams, got {}",
            branches[0].len()
        );
    }

    #[test]
    fn alternation_yields_one_branch_per_literal() {
        let branches = required_trigram_clauses("(alpha|bravo)").expect("usable literals");
        assert_eq!(branches.len(), 2, "two branches expected, got {branches:?}");
        let sets: Vec<_> = branches.iter().collect();
        assert!(sets.contains(&&trigrams("alpha")));
        assert!(sets.contains(&&trigrams("bravo")));
        // The branches must NOT be merged into one set.
        assert_ne!(branches[0], branches[1]);
    }

    /// A branch with no usable trigram cannot constrain the search, so the
    /// whole prefilter must be abandoned rather than silently narrowed.
    #[test]
    fn a_branch_without_trigrams_disables_the_prefilter() {
        assert!(required_trigram_clauses("(alpha|xy)").is_none());
    }

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
                frontmatter_raw: None,
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
        assert_eq!(second.scopes_unchanged, 0);

        let identity = store.publication_identity().unwrap().unwrap();
        let metadata = RegexIndex::new(root).list_metadata().unwrap();
        assert!(metadata.failures.is_empty());
        assert_eq!(metadata.metadata.len(), 2);
        assert!(metadata.metadata.iter().all(|entry| {
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

        let repaired = store.refresh_trigram_index(false).unwrap();
        assert_eq!(
            repaired.scopes_refreshed, 1,
            "refresh must rebuild only the scope with the corrupt selector"
        );
        let result = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(!result.scanned_fallback);
        assert_eq!(result.ready_scopes, 2);
        assert_eq!(result.dirty_scopes, 0);
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
    fn count_patterns_counts_occurrences_not_nodes() {
        // nw-300 / F-VAULT-4: 449 occurrences in one file were reported as
        // `total_matches: 2` because the verification loop increments once per
        // matching NODE. Five occurrences inside ONE section must count as 5.
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&Note {
                uid: "note:v:1".to_string(),
                vault_uid: "vlt:v".to_string(),
                file_path: "notes/dup.md".to_string(),
                title: "Dup".to_string(),
                note_kind: NoteKind::General,
                word_count: 0,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
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
                start_line: 1,
                end_line: 3,
                text_hash: "th".to_string(),
                text_content: "identical identical identical identical identical".to_string(),
                word_count: 5,
                pagerank_score: None,
            })
            .unwrap();

        let counts = store
            .count_patterns(&["identical".to_string()], None, None)
            .unwrap();
        assert_eq!(counts[0].files_matched, 1);
        assert_eq!(
            counts[0].total_matches, 5,
            "total_matches must count occurrences, not the number of nodes \
             containing at least one match"
        );
        assert_eq!(
            counts[0].top_files[0].count, 5,
            "top_files[].count is named `count` and must be occurrence-based too"
        );
    }

    /// nw-300, the wider half: `regex_search` collapses the same way via
    /// `re.find` (first match only), and unlike `count_patterns` it discloses
    /// this on NEITHER surface. Five occurrences in one node must produce five
    /// results with five distinct line numbers, not one.
    #[test]
    fn regex_search_returns_every_occurrence_within_a_node() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&Note {
                uid: "note:v:1".to_string(),
                vault_uid: "vlt:v".to_string(),
                file_path: "notes/dup.md".to_string(),
                title: "Dup".to_string(),
                note_kind: NoteKind::General,
                word_count: 0,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
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
                start_line: 10,
                end_line: 12,
                text_hash: "th".to_string(),
                text_content: "identical\nidentical\nidentical".to_string(),
                word_count: 3,
                pagerank_score: None,
            })
            .unwrap();

        let res = store
            .regex_search("identical", None, None, Some(10_000), Some(5_000))
            .unwrap();
        assert_eq!(
            res.results.len(),
            3,
            "regex_search reports one result per NODE; `line`/`snippet` describe \
             the FIRST occurrence only and `truncated` stays false (nw-300)"
        );
        let lines: Vec<Option<u32>> = res.results.iter().map(|m| m.line).collect();
        assert_eq!(
            lines,
            vec![Some(10), Some(11), Some(12)],
            "each occurrence must carry its own file line"
        );
        assert!(!res.truncated);
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

    /// nw-335's corrupt-row tolerance made the whole-corpus scans this module
    /// builds its candidate set from come back SHORT without coming back
    /// `Err`. This module's contract is that a partial answer says it is
    /// partial — `Deadline` and `CandidateCap` exist for that, and nw-076 was
    /// this surface reporting a partial scan as definitive — so a corpus that
    /// lost rows must not be reported as `truncated: false`.
    ///
    /// The second half matters as much as the first: the disclosure must not
    /// come at the cost of the results. `truncated` breaks the verification
    /// loop, so routing the corpus signal through it would return ZERO matches
    /// on one unreadable row.
    #[test]
    fn a_corpus_that_lost_rows_is_reported_partial_and_still_returns_its_matches() {
        let store = store_with_text();
        // The clean corpus answers definitively.
        let clean = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(!clean.results.is_empty(), "baseline must match");
        assert!(
            !clean.truncated && clean.truncation_reason.is_none(),
            "a corpus read in full is NOT partial, or the signal is worthless: {clean:?}"
        );

        // Poison one unrelated section with a NUL. The store now skips it.
        store
            .insert_section(&Section {
                uid: "sec:v:1:corrupt".to_string(),
                note_uid: "note:v:1".to_string(),
                heading_uid: None,
                start_line: 20,
                end_line: 21,
                text_hash: "tc".to_string(),
                text_content: "unrelated\u{0}poison".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();

        let degraded = store
            .regex_search("authenticateUser", None, None, None, None)
            .unwrap();
        assert!(
            !degraded.results.is_empty(),
            "one unreadable row must not cost every match — that is the \
             nw-076 failure, not a fix for it: {degraded:?}"
        );
        assert!(
            degraded.truncated,
            "a scan over a corpus with unread rows has not established that \
             no further matches exist: {degraded:?}"
        );
        assert_eq!(
            degraded.truncation_reason,
            Some(RegexTruncationReason::UndecodableRows),
            "the caller is told WHICH kind of incompleteness this is: {degraded:?}"
        );
    }

    /// A genuine stop is the reason a caller can act on; a short corpus must
    /// still survive when nothing else stopped the scan.
    #[test]
    fn a_real_stop_outranks_a_short_corpus_but_a_short_corpus_outranks_nothing() {
        use RegexTruncationReason::{CandidateCap, Deadline, UndecodableRows};
        assert_eq!(
            stronger_truncation(Some(UndecodableRows), Some(Deadline)),
            Some(Deadline)
        );
        assert_eq!(
            stronger_truncation(Some(CandidateCap), Some(UndecodableRows)),
            Some(CandidateCap)
        );
        assert_eq!(
            stronger_truncation(Some(UndecodableRows), None),
            Some(UndecodableRows),
            "nothing else stopped the scan, so the short corpus IS the reason"
        );
        assert_eq!(stronger_truncation(None, None), None);
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
        assert_eq!(refresh.scopes_unchanged, 0);
        assert_eq!(refresh.nodes_added, 1);
        let unchanged = store.refresh_trigram_index(false).unwrap();
        assert_eq!(unchanged.scopes_refreshed, 0);
        assert_eq!(unchanged.postings_added, 0);
        assert_eq!(unchanged.scopes_unchanged, 0);
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
        store.mark_regex_scope_dirty("repo:1", false).unwrap();
        store.mark_regex_scope_dirty("repo:0", false).unwrap();
        let refresh = store.refresh_trigram_index(false).unwrap();
        assert_eq!(refresh.nodes_added, 1);
        assert_eq!(refresh.nodes_changed, 0);
        // The emptied source scope is retired as a tombstone rather than
        // opening its old manifest only to count deleted documents.
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
            .metadata
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
                frontmatter_raw: None,
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
                frontmatter_raw: None,
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

    /// Manual perf harness for the post-nw-427 verification-budget
    /// regression. Deliberately NOT part of the default suite: it is
    /// real-wall-clock and sized to genuinely exercise `DEFAULT_MAX_MILLIS`
    /// (multi-second unbounded cost), so it is slow and mildly
    /// hardware-sensitive by nature -- exactly the property a fixed-corpus,
    /// fixed-assertion test in the default suite must NOT have. Run
    /// explicitly:
    ///
    /// ```text
    /// cargo test -p nestweaver-store --release --lib \
    ///   regex::tests::perf_verification_budget_stays_within_the_callers_deadline \
    ///   -- --ignored --nocapture
    /// ```
    ///
    /// Fixture adequacy: uses the exact pattern from the measured regression
    /// (`\d{4}-\d{2}-\d{2}`, no literal trigram, so this always hits the same
    /// full-scan path real callers hit) over a corpus sized so the unbounded
    /// (`--max-millis 120000`) run takes multiple seconds -- comfortably past
    /// `DEFAULT_MAX_MILLIS` (2000ms), so the uncapped-default, generous, and
    /// near-cliff scenarios below land in genuinely different places rather
    /// than all finishing instantly regardless of which budget logic is in
    /// use (asserted directly below, not assumed).
    #[test]
    #[ignore = "wall-clock perf harness; run explicitly with --ignored --nocapture"]
    fn perf_verification_budget_stays_within_the_callers_deadline() {
        let store = GraphStore::in_memory().unwrap();
        // ~4000 symbols x ~2.6KB signature, each with 8 embedded date-shaped
        // occurrences: big enough that collecting + regex-scanning the whole
        // corpus unbounded takes multiple seconds on ordinary hardware.
        const SYMS: usize = 4000;
        const OCCURRENCES_PER_SIG: usize = 8;
        for i in 0..SYMS {
            let mut sig = String::with_capacity(2600);
            for j in 0..OCCURRENCES_PER_SIG {
                sig.push_str(&format!(
                    "// entry {i}-{j} recorded 20{:02}-{:02}-{:02} during batch \
                     processing of request payload; ",
                    i % 100,
                    (j % 12) + 1,
                    (i % 28) + 1,
                ));
                sig.push_str("filler filler filler filler filler filler filler filler ");
            }
            store
                .insert_symbol(&Symbol {
                    uid: format!("sym:perf:{i}"),
                    name: format!("perfSymbol{i}"),
                    kind: SymbolKind::Function,
                    repo_uid: "repo:perf".to_string(),
                    file_path: format!("src/perf_{i}.rs"),
                    start_line: 1,
                    end_line: 2,
                    signature: sig,
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

        let pattern = r"\d{4}-\d{2}-\d{2}";

        // Effectively unbounded, to learn the real un-truncated cost this
        // corpus imposes -- the yardstick the capped scenarios below are
        // judged against.
        let unbounded = store
            .regex_search(pattern, None, None, None, Some(120_000))
            .unwrap();
        eprintln!(
            "[perf] unbounded:                truncated={} results={:>6} timings={:?}",
            unbounded.truncated,
            unbounded.results.len(),
            unbounded.timings
        );
        assert!(
            !unbounded.results.is_empty(),
            "fixture adequacy: the corpus must actually contain matches, or \
             none of the assertions below mean anything"
        );
        assert!(
            !unbounded.truncated,
            "120s must be enough to finish this corpus, or the yardstick \
             itself is truncated"
        );

        for (label, max_millis) in [
            ("uncapped default (None)", None),
            ("generous (500ms)", Some(500)),
            ("near-cliff (25ms)", Some(25)),
        ] {
            let result = store
                .regex_search(pattern, None, None, None, max_millis)
                .unwrap();
            eprintln!(
                "[perf] {label:<24}: truncated={} reason={:?} results={:>6} timings={:?}",
                result.truncated,
                result.truncation_reason,
                result.results.len(),
                result.timings
            );
            let budget = max_millis.unwrap_or(DEFAULT_MAX_MILLIS);
            if unbounded.timings.total_ms > budget {
                assert!(
                    !result.results.is_empty(),
                    "{label}: a budget too small to finish this corpus must \
                     still return whatever was already verified, not discard \
                     it -- that is the pre-nw-427 bug: {result:?}"
                );
                // INVARIANT under test: total wall time stays near the
                // caller's own budget, not ~2x it. Generous slack (2x the
                // budget plus a fixed floor) keeps this from flaking on a
                // loaded machine while still catching the regression this
                // fixes -- reverting `verification_budget_ms` to return the
                // full `deadline_ms` regardless of elapsed time lets total_ms
                // approach `deadline_ms` (collection) + the full unbounded
                // verification cost, which this bound is sized to catch.
                assert!(
                    result.timings.total_ms <= budget.saturating_mul(2) + 200,
                    "{label}: total_ms={} should stay near budget={budget}ms, \
                     not run up toward the unbounded cost of {}ms: {result:?}",
                    result.timings.total_ms,
                    unbounded.timings.total_ms,
                );
            }
        }
    }
}
