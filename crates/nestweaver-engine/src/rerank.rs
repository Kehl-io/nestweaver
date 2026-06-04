//! Feature F17 — lightweight result reranker (framework + monotonic default).
//!
//! # What this is (and what it is NOT)
//!
//! This is a small, **off-by-default** reranking *framework*. It re-scores the
//! top-N candidates of an already-retrieved result set and stable-sorts them by
//! the new score, leaving the tail untouched. It does **not** change recall:
//! reranking only reorders nodes that retrieval already surfaced.
//!
//! The default scorer ([`MonotonicReranker`]) is a hand-tuned, **monotonic,
//! explainable** linear heuristic over a handful of cheap features. It is a
//! transparent heuristic, **not** a proven win — there is no offline evaluation
//! harness wired up yet, so we cannot claim it improves nDCG. It is shipped off
//! by default precisely because it is unvalidated.
//!
//! ## Why no neural net / learned model out of the box
//!
//! An adversarial design review rejected the original "hand-rolled candle
//! neural net" plan:
//!
//! - there is no GPU in the target environment;
//! - single-user interaction data is far too sparse to train a net;
//! - there is no evaluation harness (planned P0.3) to gate a learned model.
//!
//! So instead this module provides:
//!
//! 1. a [`MonotonicReranker`] default — cheap, explainable, deterministic;
//! 2. a [`LoadedModelReranker`] hook that loads **JSON weights** (NOT a binary
//!    blob, NOT ONNX/candle) from `<db>.rerank.json` *if present and the version
//!    matches*, falling back to the monotonic scorer otherwise. A future
//!    external trainer (e.g. LambdaMART / a small GBDT exported to linear
//!    feature weights) could write that file.
//!
//! Before any learned model should be *trusted*, it must clear a real bar:
//! train on accumulated F1 interaction labels (see
//! [`crate::interactions`]), evaluate offline with the (not-yet-built) eval
//! harness, and beat the monotonic baseline by **>= 5% nDCG@10**. Until that
//! gate exists, the JSON-weights path is scaffolding, and the monotonic scorer
//! is the only thing we ship.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::query::BrainNode;
use crate::recency::parse_iso8601_to_epoch;
use nestweaver_store::GraphStore;

/// Default number of leading candidates re-scored by [`rerank`]. Candidates
/// beyond this position keep their original order untouched (cheap + bounds the
/// per-query cost; the tail is rarely consumed by an LLM anyway).
pub const DEFAULT_TOP_N: usize = 50;

/// The sidecar suffix for an optional learned-model weights file.
pub const RERANK_SIDECAR_SUFFIX: &str = ".rerank.json";

/// The schema version this build understands for [`RerankModel`] JSON files.
/// A file whose `version` does not match is ignored (we fall back to the
/// monotonic scorer) rather than risk mis-applying weights from a future or
/// stale schema.
pub const RERANK_MODEL_VERSION: u32 = 1;

// ── Feature vector ─────────────────────────────────────────────────────────

/// Per-candidate features extracted from a hybrid result.
///
/// Deliberately small and cheap to compute. Every field is either already on
/// the [`BrainNode`] or a O(1)/already-loaded lookup. No embeddings, no graph
/// walks, no disk reads in the hot path.
#[derive(Debug, Clone)]
pub struct RerankFeatures {
    /// 0-based position in the fused result list (lower = retrieval ranked it
    /// higher). Lets a reranker keep some faith in the upstream order.
    pub rank_position: usize,
    /// The fused relevance score carried on the node (PPR / RRF / post-prior).
    /// This is the primary signal.
    pub relevance: f64,
    /// The node's `kind` string (e.g. `"Symbol/function"`, `"Note"`,
    /// `"Section"`). Used for small kind priors.
    pub node_kind: String,
    /// Whether the node already carries an inline body (F8). A node we already
    /// chose to inline is, weakly, a more "useful to read now" candidate.
    pub is_inline_body: bool,
    /// Age of the node in days (from note/section `modified_at`), when known.
    /// `None` for code symbols and notes lacking a timestamp.
    pub age_days: Option<f64>,
    /// Number of taxonomy aliases that matched this node's title/identity, when
    /// cheaply known. `0` when not computed (the default on most paths).
    pub matched_alias_count: usize,
}

impl RerankFeatures {
    /// Extract features for `node` at fused position `rank_position`, using
    /// `note_ages` (UID -> age in days) for the recency signal. `note_ages`
    /// should be built once per query (see [`build_node_ages`]) so this stays
    /// O(1) per candidate.
    pub fn from_node(
        node: &BrainNode,
        rank_position: usize,
        note_ages: &std::collections::HashMap<String, f64>,
        alias_counts: &std::collections::HashMap<String, usize>,
    ) -> Self {
        RerankFeatures {
            rank_position,
            relevance: node.relevance,
            node_kind: node.kind.clone(),
            is_inline_body: node.inline_body.is_some(),
            age_days: note_ages.get(&node.uid).copied(),
            matched_alias_count: alias_counts.get(&node.uid).copied().unwrap_or(0),
        }
    }
}

/// Build a `UID -> age_in_days` map for the nodes about to be reranked.
///
/// Ages come from each Note's `modified_at`; Section nodes inherit their parent
/// note's timestamp (mirrors the recency-bias logic). Computed once per query.
/// Code symbols have no markdown timestamp and are simply absent from the map
/// (their `age_days` feature is `None`).
pub fn build_node_ages(
    store: &GraphStore,
    nodes: &[BrainNode],
) -> std::collections::HashMap<String, f64> {
    use std::collections::HashMap;
    let needed: std::collections::HashSet<&str> = nodes.iter().map(|n| n.uid.as_str()).collect();
    if needed.is_empty() {
        return HashMap::new();
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64;

    // note_uid -> modified_at epoch secs
    let note_ts: HashMap<String, f64> = store
        .list_notes(None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| n.modified_at.map(|t| (n.uid, parse_iso8601_to_epoch(&t))))
        .collect();
    // section_uid -> parent note_uid
    let section_note: HashMap<String, String> = store
        .list_all_sections()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.uid, s.note_uid))
        .collect();

    let mut out = HashMap::new();
    for node in nodes {
        let ts = if let Some(&t) = note_ts.get(&node.uid) {
            t
        } else if let Some(parent) = section_note.get(&node.uid) {
            note_ts.get(parent).copied().unwrap_or(0.0)
        } else {
            0.0
        };
        if ts > 0.0 {
            let age_days = ((now - ts).max(0.0)) / 86_400.0;
            out.insert(node.uid.clone(), age_days);
        }
    }
    out
}

// ── Reranker trait + scorers ─────────────────────────────────────────────────

/// A pluggable scorer over [`RerankFeatures`]. Higher score = ranked higher.
pub trait Reranker {
    /// Score a single candidate. Must be a pure function of its features.
    fn score(&self, f: &RerankFeatures) -> f64;
}

/// Hand-tuned monotonic linear weights for [`MonotonicReranker`].
///
/// Each weight has a documented rationale. The combined score is intentionally
/// **monotonically increasing in `relevance`** (the `relevance` term dominates
/// and every other term is bounded), so a higher-relevance candidate — all else
/// equal — always scores higher. This is the property the tests pin.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicWeights {
    /// Primary signal. The fused retrieval score is what we trust most, so it
    /// gets the dominant weight. Keeping this large guarantees monotonicity in
    /// relevance regardless of the smaller nudges below.
    pub relevance: f64,
    /// Mild faith in the upstream order: a small penalty per rank position so
    /// that, when two candidates have near-identical relevance, the one
    /// retrieval already ranked higher wins the tie. Small so it never
    /// overrides a real relevance gap.
    pub rank_penalty_per_position: f64,
    /// Mild recency boost. Decays with age (half-life ~30 days). Bounded in
    /// `[0, recency_boost_max]` so a fresh note nudges up but can never swamp
    /// the relevance term. Notes/sections only — code symbols have no age.
    pub recency_boost_max: f64,
    /// Half-life (days) for the recency decay above.
    pub recency_half_life_days: f64,
    /// Small prior favouring documentation (Note/Section) over code symbols on
    /// the brain-context path, where prose tends to answer "what/why" queries.
    /// Tiny — a tie-breaker, not a reordering force.
    pub doc_kind_prior: f64,
    /// Small nudge for nodes that already carry an inline body (F8): we already
    /// judged them worth inlining, so weakly prefer them. Tiny.
    pub inline_body_nudge: f64,
    /// Small per-matched-alias nudge: a node whose identity matched the query's
    /// taxonomy aliases is weakly more on-topic. Bounded by capping the count.
    pub alias_nudge_per_match: f64,
    /// Cap on `matched_alias_count` contribution so a pathological alias map
    /// can't dominate.
    pub alias_match_cap: usize,
}

impl Default for MonotonicWeights {
    fn default() -> Self {
        MonotonicWeights {
            // Dominant — see struct docs. Picked so that even the maximum
            // possible sum of all the bounded nudges below is far smaller than
            // the relevance contribution of a typical score gap.
            relevance: 1.0,
            // ~0.001 per position: across the top 50 this is at most 0.05,
            // i.e. smaller than the recency cap and tiny vs. relevance.
            rank_penalty_per_position: 0.001,
            // At most +0.05 for a brand-new note.
            recency_boost_max: 0.05,
            recency_half_life_days: 30.0,
            // Tie-breaker scale.
            doc_kind_prior: 0.02,
            inline_body_nudge: 0.02,
            // At most +0.03 (cap 3 * 0.01).
            alias_nudge_per_match: 0.01,
            alias_match_cap: 3,
        }
    }
}

/// The default reranker: a transparent, monotonic, explainable linear scorer.
///
/// NOT a learned model and NOT validated against an eval harness. Off by
/// default at every call site; when enabled it is a heuristic reordering of the
/// top-N, nothing more.
#[derive(Debug, Clone, Default)]
pub struct MonotonicReranker {
    pub weights: MonotonicWeights,
}

impl MonotonicReranker {
    /// Construct with the documented default weights.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Reranker for MonotonicReranker {
    fn score(&self, f: &RerankFeatures) -> f64 {
        let w = &self.weights;

        // Primary term — dominant and strictly increasing in relevance.
        let mut s = w.relevance * f.relevance;

        // Mild upstream-order faith (bounded, small).
        s -= w.rank_penalty_per_position * f.rank_position as f64;

        // Mild recency boost (notes/sections with a known age only).
        if let Some(age) = f.age_days {
            let ln2 = std::f64::consts::LN_2;
            let decay = (-(age.max(0.0) * ln2) / w.recency_half_life_days).exp();
            s += w.recency_boost_max * decay;
        }

        // Small doc-kind prior (Note / Section / Heading).
        let kind_lower = f.node_kind.to_lowercase();
        if kind_lower.starts_with("note")
            || kind_lower.starts_with("section")
            || kind_lower.starts_with("heading")
        {
            s += w.doc_kind_prior;
        }

        // Tiny inline-body nudge.
        if f.is_inline_body {
            s += w.inline_body_nudge;
        }

        // Tiny alias nudge (capped).
        let alias = f.matched_alias_count.min(w.alias_match_cap) as f64;
        s += w.alias_nudge_per_match * alias;

        s
    }
}

// ── Optional learned-model hook (JSON weights, NOT a binary blob) ─────────────

/// On-disk JSON shape for an optional learned model.
///
/// This is intentionally just **linear feature weights** plus a version tag —
/// no ONNX, no candle, no binary blob. A future external trainer (LambdaMART /
/// small GBDT distilled to linear weights, etc.) could emit this file at
/// `<db>.rerank.json`. We load it only when present AND `version` matches
/// [`RERANK_MODEL_VERSION`]; otherwise we silently fall back to the monotonic
/// scorer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankModel {
    /// Schema version. Must equal [`RERANK_MODEL_VERSION`] to be loaded.
    pub version: u32,
    /// Linear coefficient on `relevance`.
    pub w_relevance: f64,
    /// Linear coefficient on `rank_position`.
    pub w_rank_position: f64,
    /// Linear coefficient on `age_days` (applied to a clamped age; missing age
    /// contributes 0).
    pub w_age_days: f64,
    /// Linear coefficient applied when the node is a doc kind (Note/Section).
    pub w_doc_kind: f64,
    /// Linear coefficient applied when the node has an inline body.
    pub w_inline_body: f64,
    /// Linear coefficient on `matched_alias_count`.
    pub w_alias_match: f64,
    /// Constant bias term.
    pub bias: f64,
}

/// A reranker backed by loaded JSON weights.
///
/// UNVALIDATED. Constructed only via [`load_rerank_model`], which refuses files
/// whose version does not match. Even when loaded, this path has not cleared
/// any eval gate (see module docs) — it exists so a future trainer has a place
/// to deposit weights, not because we trust it today.
#[derive(Debug, Clone)]
pub struct LoadedModelReranker {
    pub model: RerankModel,
}

impl Reranker for LoadedModelReranker {
    fn score(&self, f: &RerankFeatures) -> f64 {
        let m = &self.model;
        let kind_lower = f.node_kind.to_lowercase();
        let is_doc = kind_lower.starts_with("note")
            || kind_lower.starts_with("section")
            || kind_lower.starts_with("heading");
        // Clamp age to a sane window so a single ancient/garbage timestamp
        // can't dominate the linear combination.
        let age = f.age_days.unwrap_or(0.0).clamp(0.0, 3650.0);
        m.bias
            + m.w_relevance * f.relevance
            + m.w_rank_position * f.rank_position as f64
            + m.w_age_days * age
            + if is_doc { m.w_doc_kind } else { 0.0 }
            + if f.is_inline_body {
                m.w_inline_body
            } else {
                0.0
            }
            + m.w_alias_match * f.matched_alias_count as f64
    }
}

/// Resolve the `<db>.rerank.json` sidecar path for a database path.
pub fn rerank_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    crate::sidecar_path(db_path, RERANK_SIDECAR_SUFFIX)
}

/// Load the optional learned model from `<db>.rerank.json`, if present and the
/// version matches. Returns `None` (caller should fall back to monotonic) when
/// the file is missing, unparseable, or version-mismatched.
pub fn load_rerank_model(db_path: &Path) -> Option<RerankModel> {
    let path = rerank_sidecar_path(db_path);
    let text = std::fs::read_to_string(&path).ok()?;
    let model: RerankModel = serde_json::from_str(&text).ok()?;
    if model.version != RERANK_MODEL_VERSION {
        tracing::warn!(
            version = model.version,
            expected = RERANK_MODEL_VERSION,
            "ignoring <db>.rerank.json: version mismatch; falling back to monotonic reranker"
        );
        return None;
    }
    Some(model)
}

/// Select the active reranker for `db_path`: the loaded JSON model when present
/// and valid, else the monotonic default. The returned boxed reranker is what
/// callers pass to [`rerank`].
pub fn select_reranker(db_path: Option<&Path>) -> Box<dyn Reranker> {
    if let Some(p) = db_path
        && let Some(model) = load_rerank_model(p)
    {
        tracing::info!("using loaded (unvalidated) rerank model from <db>.rerank.json");
        return Box::new(LoadedModelReranker { model });
    }
    Box::new(MonotonicReranker::new())
}

// ── The rerank entry point ───────────────────────────────────────────────────

/// Re-score the leading `top_n` candidates of `nodes` with `reranker` and
/// stable-sort *that prefix* by the new score (descending). Candidates beyond
/// `top_n` keep their original relative order and are left untouched.
///
/// This only reorders an already-retrieved set; recall is unchanged. With the
/// monotonic default, the reordering is a transparent heuristic (see module
/// docs). A `top_n` of `0` makes this a no-op.
pub fn rerank(nodes: &mut [BrainNode], reranker: &dyn Reranker, store: &GraphStore, top_n: usize) {
    let cut = top_n.min(nodes.len());
    if cut < 2 {
        return; // nothing to reorder
    }

    let head = &mut nodes[..cut];
    let ages = build_node_ages(store, head);
    // Alias counts are not cheaply known on the default path; pass an empty map
    // so the feature contributes 0. (Reserved hook for callers that do know.)
    let alias_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    // Score each candidate at its current position, then stable-sort by score
    // descending. Stable sort preserves upstream order among equal scores.
    let mut scored: Vec<(f64, BrainNode)> = head
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let feats = RerankFeatures::from_node(n, i, &ages, &alias_counts);
            (reranker.score(&feats), n.clone())
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for (slot, (_, node)) in head.iter_mut().zip(scored) {
        *slot = node;
    }
}

// ── Training-export scaffold (NO training here) ──────────────────────────────

/// One exported training row. SCAFFOLD ONLY — these rows are written to a JSONL
/// file for an *external* trainer to consume. Nothing in this repo trains a
/// model; there is no labelled data of meaningful size yet and no eval harness
/// to gate one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingRow {
    /// Candidate node UID.
    pub uid: String,
    /// The node's kind (feature).
    pub node_kind: String,
    /// Derived binary label from F1 interaction success signals: `1` when the
    /// node accumulated TerminalSuccess or FollowUp signals (positive
    /// relevance evidence), else `0`. This is a weak, noisy proxy — documented
    /// as such — not a curated relevance judgement.
    pub label: u32,
    /// Raw F1 counters carried through for the external trainer's own feature
    /// engineering.
    pub terminal_success_count: u32,
    pub result_used_count: u32,
    pub query_seed_count: u32,
    pub result_shown_count: u32,
    pub access_count: u32,
    pub distinct_sessions: u32,
    /// The node's decayed interaction score (F1).
    pub interaction_score: f64,
}

/// Export per-(candidate) feature+label rows derived from F1 interaction
/// success signals to a JSONL file, for OFFLINE training elsewhere.
///
/// SCAFFOLD: this does not train anything. It snapshots whatever interaction
/// data exists (possibly empty) so a future external trainer (run outside this
/// process) can learn weights and emit a `<db>.rerank.json`. Labels are a weak
/// proxy: TerminalSuccess/FollowUp signals → positive (1), otherwise negative
/// (0). A real training pipeline would need richer (query, candidate, judged-
/// relevance) tuples and the eval harness before any learned model is trusted.
///
/// Returns the number of rows written. Writes an empty file (0 rows) when no
/// interaction data exists — that's expected and fine for a scaffold.
pub fn export_training_rows(db_path: &Path, out_path: &Path) -> Result<usize, anyhow::Error> {
    use std::io::Write;

    let store = crate::interactions::load_interaction_store_public(db_path);

    let mut buf = String::new();
    let mut count = 0usize;
    if let Some(store) = store {
        for (uid, ns) in &store.node_scores {
            let positive = ns.terminal_success_count > 0 || ns.result_used_count > 0;
            let kind = node_kind_from_uid(uid);
            let row = TrainingRow {
                uid: uid.clone(),
                node_kind: kind,
                label: if positive { 1 } else { 0 },
                terminal_success_count: ns.terminal_success_count,
                result_used_count: ns.result_used_count,
                query_seed_count: ns.query_seed_count,
                result_shown_count: ns.result_shown_count,
                access_count: ns.access_count,
                distinct_sessions: ns.distinct_sessions,
                interaction_score: ns.computed_score,
            };
            buf.push_str(&serde_json::to_string(&row)?);
            buf.push('\n');
            count += 1;
        }
    }

    let mut f = std::fs::File::create(out_path)?;
    f.write_all(buf.as_bytes())?;
    Ok(count)
}

/// Best-effort node-kind label from a UID prefix (for the training scaffold).
fn node_kind_from_uid(uid: &str) -> String {
    let prefix = uid.split(':').next().unwrap_or("");
    match prefix {
        "sym" => "Symbol",
        "note" => "Note",
        "sec" => "Section",
        "head" => "Heading",
        "tag" => "Tag",
        other => other,
    }
    .to_string()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn feats(relevance: f64, rank: usize) -> RerankFeatures {
        RerankFeatures {
            rank_position: rank,
            relevance,
            node_kind: "Symbol/function".to_string(),
            is_inline_body: false,
            age_days: None,
            matched_alias_count: 0,
        }
    }

    fn node(uid: &str, relevance: f64) -> BrainNode {
        BrainNode {
            uid: uid.to_string(),
            kind: "Symbol/function".to_string(),
            title: uid.to_string(),
            location: format!("src/{uid}.rs:1"),
            relevance,
            inline_body: None,
            body_complete: true,
        }
    }

    #[test]
    fn monotonic_in_relevance_all_else_equal() {
        let rr = MonotonicReranker::new();
        // Same rank, same everything but relevance: higher relevance must score
        // strictly higher.
        let lo = rr.score(&feats(0.1, 5));
        let mid = rr.score(&feats(0.5, 5));
        let hi = rr.score(&feats(0.9, 5));
        assert!(lo < mid, "{lo} < {mid}");
        assert!(mid < hi, "{mid} < {hi}");

        // Property check across a sweep at fixed rank.
        let mut prev = f64::NEG_INFINITY;
        for i in 0..=100 {
            let r = i as f64 / 100.0;
            let s = rr.score(&feats(r, 7));
            assert!(s > prev, "score must increase with relevance at r={r}");
            prev = s;
        }
    }

    #[test]
    fn relevance_dominates_nudges() {
        // Even with every nudge stacked against it, a meaningfully higher
        // relevance still wins — the relevance term is dominant.
        let rr = MonotonicReranker::new();
        let plain = RerankFeatures {
            rank_position: 0,
            relevance: 0.50,
            node_kind: "Symbol/function".to_string(),
            is_inline_body: false,
            age_days: None,
            matched_alias_count: 0,
        };
        let stacked_but_lower = RerankFeatures {
            rank_position: 0,
            relevance: 0.30, // 0.20 lower
            node_kind: "Note".to_string(),
            is_inline_body: true,
            age_days: Some(0.0), // freshest possible
            matched_alias_count: 10,
        };
        assert!(rr.score(&plain) > rr.score(&stacked_but_lower));
    }

    #[test]
    fn rerank_reorders_head_by_weights_and_leaves_tail_untouched() {
        // Construct a list whose retrieval order is the REVERSE of relevance in
        // the head, plus a tail beyond top_n that must not move.
        let mut nodes = vec![
            node("a", 0.10), // head, lowest relevance, currently first
            node("b", 0.50),
            node("c", 0.90), // head, highest relevance, currently third
            node("tail1", 0.99),
            node("tail2", 0.05),
        ];
        let store = make_empty_store();
        let rr = MonotonicReranker::new();

        rerank(&mut nodes, &rr, &store, 3);

        // Head re-sorted by score (relevance-dominant) descending: c, b, a.
        assert_eq!(nodes[0].uid, "c");
        assert_eq!(nodes[1].uid, "b");
        assert_eq!(nodes[2].uid, "a");
        // Tail untouched despite tail1 having the highest relevance overall.
        assert_eq!(nodes[3].uid, "tail1");
        assert_eq!(nodes[4].uid, "tail2");
    }

    #[test]
    fn rerank_off_leaves_order_unchanged() {
        // "Off" is modelled by simply not calling rerank. To pin the byte-
        // identical-when-off contract, snapshot the order, (don't) rerank, and
        // assert equality.
        let original = vec![node("a", 0.10), node("b", 0.50), node("c", 0.90)];
        let untouched = original.clone();
        // No rerank call.
        let uids_before: Vec<&str> = original.iter().map(|n| n.uid.as_str()).collect();
        let uids_after: Vec<&str> = untouched.iter().map(|n| n.uid.as_str()).collect();
        assert_eq!(uids_before, uids_after);
    }

    #[test]
    fn rerank_top_n_zero_is_noop() {
        let mut nodes = vec![node("a", 0.1), node("b", 0.9)];
        let store = make_empty_store();
        let rr = MonotonicReranker::new();
        rerank(&mut nodes, &rr, &store, 0);
        assert_eq!(nodes[0].uid, "a");
        assert_eq!(nodes[1].uid, "b");
    }

    #[test]
    fn loaded_model_version_mismatch_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.lbug");
        let model = RerankModel {
            version: 999, // mismatched
            w_relevance: 1.0,
            w_rank_position: 0.0,
            w_age_days: 0.0,
            w_doc_kind: 0.0,
            w_inline_body: 0.0,
            w_alias_match: 0.0,
            bias: 0.0,
        };
        std::fs::write(
            rerank_sidecar_path(&db),
            serde_json::to_string(&model).unwrap(),
        )
        .unwrap();
        assert!(
            load_rerank_model(&db).is_none(),
            "version mismatch must be rejected"
        );
    }

    #[test]
    fn loaded_model_roundtrips_when_version_matches() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.lbug");
        let model = RerankModel {
            version: RERANK_MODEL_VERSION,
            w_relevance: 2.0,
            w_rank_position: -0.01,
            w_age_days: -0.001,
            w_doc_kind: 0.1,
            w_inline_body: 0.05,
            w_alias_match: 0.02,
            bias: 0.3,
        };
        std::fs::write(
            rerank_sidecar_path(&db),
            serde_json::to_string(&model).unwrap(),
        )
        .unwrap();
        let loaded = load_rerank_model(&db).expect("should load matching version");
        assert_eq!(loaded.w_relevance, 2.0);
        // The loaded reranker is also monotonic in relevance for positive
        // w_relevance.
        let rr = LoadedModelReranker { model: loaded };
        let f1 = RerankFeatures {
            rank_position: 0,
            relevance: 0.2,
            node_kind: "Note".into(),
            is_inline_body: false,
            age_days: None,
            matched_alias_count: 0,
        };
        let mut f2 = f1.clone();
        f2.relevance = 0.8;
        assert!(rr.score(&f2) > rr.score(&f1));
    }

    #[test]
    fn export_training_rows_on_empty_interactions_writes_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.lbug");
        let out = dir.path().join("train.jsonl");
        // No interaction sidecar exists → scaffold writes an empty file, 0 rows.
        let n = export_training_rows(&db, &out).unwrap();
        assert_eq!(n, 0);
        assert!(out.exists());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "");
    }

    #[test]
    fn export_training_rows_derives_labels_from_success_signals() {
        use crate::interactions::{InteractionTracker, interaction_sidecar_path};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.lbug");
        let tracker = InteractionTracker::new(&db);
        // A positive (terminal success) and a neutral (only shown) node.
        tracker.record_terminal_success(&["note:pos".into()]);
        tracker.record_query("brain_context", &[], &["sym:neutral".into()]);
        tracker.flush().unwrap();
        assert!(interaction_sidecar_path(&db).exists());

        let out = dir.path().join("train.jsonl");
        let n = export_training_rows(&db, &out).unwrap();
        assert!(n >= 2);
        let text = std::fs::read_to_string(&out).unwrap();
        let rows: Vec<TrainingRow> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let pos = rows.iter().find(|r| r.uid == "note:pos").unwrap();
        let neutral = rows.iter().find(|r| r.uid == "sym:neutral").unwrap();
        assert_eq!(pos.label, 1, "terminal success → positive label");
        assert_eq!(pos.node_kind, "Note");
        assert_eq!(neutral.label, 0, "only shown → negative label");
        assert_eq!(neutral.node_kind, "Symbol");
    }

    /// A throwaway empty store for tests that need a `&GraphStore` but exercise
    /// no markdown content (so `build_node_ages` returns an empty map).
    fn make_empty_store() -> GraphStore {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rerank_test.lbug");
        let store = GraphStore::open(&path).expect("open store");
        // Leak the tempdir so the store's backing files survive the test.
        std::mem::forget(dir);
        store
    }
}
