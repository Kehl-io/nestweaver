//! P0.3 — offline retrieval-quality evaluation harness.
//!
//! This module scores NestWeaver's retrieval quality (nDCG@10 / MRR /
//! precision@k) over a *judged* query set. Its reason to exist is honesty:
//! the quality features (F6 ranking priors, F7 pseudo-relevance feedback,
//! F1 interaction feedback, F12 CodeRank, F17 reranking) all ship
//! **off-by-default** precisely because there was nothing to measure them
//! with. This harness is that measuring stick — run a feature on, run it off,
//! compare the metrics on the same judged set, and only turn it on if it
//! clears a defensible gate (the project uses >= 5% nDCG@10).
//!
//! ## HONEST FRAMING — read before trusting any number this produces
//!
//! - **Meaningful evaluation requires REAL human relevance labels over the
//!   ACTUAL corpus you index.** A judged query is `(query, {node-uid →
//!   graded relevance 0..=3})`. Those grades must be assigned by a human (or
//!   a carefully validated proxy) looking at *your* code/notes, not invented.
//! - **The bundled sample file is a FORMAT TEMPLATE, not a benchmark.** Its
//!   UIDs are placeholders. Running the harness against it tells you the file
//!   parses, nothing about retrieval quality.
//! - **Metrics on a tiny or synthetic set are not authoritative.** A 3-query
//!   set can swing wildly; one query flipping rank dominates the mean.
//! - **Do not trust a small mean delta.** Before believing a feature helps,
//!   look at *per-query* win/loss counts and confidence, and use time-based or
//!   query-based train/test splits so you are not tuning on the same queries
//!   you evaluate on.
//!
//! The pure metric functions ([`ndcg_at_k`], [`mrr`], [`precision_at_k`]) are
//! standard and unit-tested; the [`run_eval`] runner reuses the existing
//! hybrid retrieval ([`crate::query::build_brain_context_hybrid_with_aliases`])
//! so it scores exactly what the product ships.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use nestweaver_store::{GraphStore, QueryIntent, TantivyIndex};

use crate::query::{
    BrainContextResult, HybridSearchConfig, build_brain_context_hybrid_with_aliases,
};
use crate::rerank::{DEFAULT_TOP_N, rerank, select_reranker};

/// A single judged query: the query text, an optional retrieval intent, and a
/// map from node-UID → graded relevance in `0..=3` (0 = irrelevant, 3 = ideal).
///
/// Loadable from JSON (`Vec<JudgedQuery>`) or JSONL (one object per line) via
/// [`load_judged_queries`].
// nw-159 (secondary): `deny_unknown_fields` so a mistyped key — "relevant"
// instead of "relevance" — fails to parse instead of being swallowed by
// `serde(default)` and letting the harness print a confident 0.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgedQuery {
    /// The query / seed string fed into hybrid retrieval.
    pub query: String,
    /// Optional intent tag. Parsed leniently via `QueryIntent::from_str`;
    /// e.g. `"find-definition"`, `"architecture"`, `"impact"`. `None` →
    /// general context (PPR's standard damping).
    #[serde(default)]
    pub intent: Option<String>,
    /// node-UID → graded relevance `0..=3`. Grades above 3 are tolerated by
    /// the metrics (DCG just uses `2^rel - 1`) but the intended scale is 0..=3.
    #[serde(default)]
    pub relevance: HashMap<String, u8>,
}

impl JudgedQuery {
    /// Parse `self.intent` into a [`QueryIntent`], if present and recognized.
    /// An unrecognized string is treated as `None` (general context).
    pub fn parsed_intent(&self) -> Option<QueryIntent> {
        self.intent.as_deref().and_then(|s| s.parse().ok())
    }
}

/// Per-query scores for one judged query.
#[derive(Debug, Clone, Serialize)]
pub struct PerQueryRow {
    pub query: String,
    pub ndcg10: f64,
    pub mrr: f64,
    pub p_at_5: f64,
}

/// Aggregate evaluation report over a judged query set.
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    /// One row per judged query, in input order.
    pub per_query: Vec<PerQueryRow>,
    /// Mean nDCG@10 across all queries (0.0 when `n == 0`).
    pub mean_ndcg10: f64,
    /// Mean MRR across all queries (0.0 when `n == 0`).
    pub mean_mrr: f64,
    /// Mean precision@5 across all queries (0.0 when `n == 0`).
    pub mean_p5: f64,
    /// Number of queries evaluated.
    pub n: usize,
}

// ── Pure metrics (the TDD core) ──────────────────────────────────────────────

/// Discounted Cumulative Gain at cutoff `k`.
///
/// Standard formulation with exponential gain:
///   DCG@k = Σ_{i=0..min(k,len)} (2^rel_i - 1) / log2(i + 2)
///
/// where `rel_i` is the graded relevance of the document at rank `i` (0 for any
/// UID not present in `rel`).
fn dcg_at_k(ranked_uids: &[String], rel: &HashMap<String, u8>, k: usize) -> f64 {
    ranked_uids
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, uid)| {
            let g = *rel.get(uid).unwrap_or(&0) as f64;
            let gain = (2f64.powf(g)) - 1.0;
            let discount = ((i + 2) as f64).log2();
            gain / discount
        })
        .sum()
}

/// Ideal DCG@k — DCG of the best possible ordering of the judged grades.
///
/// Built by sorting the relevance grades descending and scoring the top `k`.
fn idcg_at_k(rel: &HashMap<String, u8>, k: usize) -> f64 {
    let mut grades: Vec<u8> = rel.values().copied().collect();
    grades.sort_unstable_by(|a, b| b.cmp(a));
    grades
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &g)| {
            let gain = (2f64.powf(g as f64)) - 1.0;
            let discount = ((i + 2) as f64).log2();
            gain / discount
        })
        .sum()
}

/// Normalized Discounted Cumulative Gain at cutoff `k` (nDCG@k = DCG@k / IDCG@k).
///
/// Returns `1.0` for a perfectly-ordered ranking (relevant docs in
/// descending-grade order at the front) and `< 1.0` for any worse order.
/// Returns `0.0` when there are no relevant docs (IDCG == 0) — this also
/// covers the empty-ranking and all-zero-relevance cases gracefully.
pub fn ndcg_at_k(ranked_uids: &[String], rel: &HashMap<String, u8>, k: usize) -> f64 {
    let idcg = idcg_at_k(rel, k);
    if idcg <= 0.0 {
        return 0.0;
    }
    dcg_at_k(ranked_uids, rel, k) / idcg
}

/// Mean Reciprocal Rank for a single ranking: `1 / rank` of the first UID with
/// graded relevance `>= 1`, where rank is 1-based. Returns `0.0` when no
/// relevant UID appears in the ranking (or the ranking is empty).
pub fn mrr(ranked_uids: &[String], rel: &HashMap<String, u8>) -> f64 {
    for (i, uid) in ranked_uids.iter().enumerate() {
        if rel.get(uid).copied().unwrap_or(0) >= 1 {
            return 1.0 / (i + 1) as f64;
        }
    }
    0.0
}

/// Precision@k: fraction of the top-`k` ranked UIDs that are relevant
/// (graded relevance `>= 1`).
///
/// The denominator is `min(k, ranked.len())` so a short ranking is not
/// penalized for positions it could never fill. Returns `0.0` for an empty
/// ranking or `k == 0`.
pub fn precision_at_k(ranked_uids: &[String], rel: &HashMap<String, u8>, k: usize) -> f64 {
    let denom = k.min(ranked_uids.len());
    if denom == 0 {
        return 0.0;
    }
    let hits = ranked_uids
        .iter()
        .take(k)
        .filter(|uid| rel.get(*uid).copied().unwrap_or(0) >= 1)
        .count();
    hits as f64 / denom as f64
}

// ── Loading ──────────────────────────────────────────────────────────────────

/// Load a judged query set from a JSON or JSONL file.
///
/// Accepts either:
/// - a JSON array of [`JudgedQuery`] objects, or
/// - JSONL: one [`JudgedQuery`] object per non-blank line.
///
/// Detection is content-based: if the first non-whitespace byte is `[` the file
/// is parsed as a JSON array, otherwise as JSONL. Blank lines in JSONL are
/// skipped. Returns a clear error if the file is missing, unreadable, empty, or
/// malformed.
pub fn load_judged_queries(path: &Path) -> Result<Vec<JudgedQuery>, anyhow::Error> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!("failed to read judged-query file '{}': {e}", path.display())
    })?;

    let trimmed = raw.trim_start();
    if trimmed.is_empty() {
        anyhow::bail!(
            "judged-query file '{}' is empty; expected a JSON array or JSONL of {{query, intent?, relevance}} objects",
            path.display()
        );
    }

    let queries: Vec<JudgedQuery> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).map_err(|e| {
            anyhow::anyhow!(
                "failed to parse judged-query JSON array in '{}': {e}",
                path.display()
            )
        })?
    } else {
        let mut out = Vec::new();
        for (lineno, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let q: JudgedQuery = serde_json::from_str(line).map_err(|e| {
                anyhow::anyhow!(
                    "failed to parse judged-query JSONL line {} in '{}': {e}",
                    lineno + 1,
                    path.display()
                )
            })?;
            out.push(q);
        }
        out
    };

    if queries.is_empty() {
        anyhow::bail!(
            "judged-query file '{}' contained no queries",
            path.display()
        );
    }

    Ok(queries)
}

// ── Runner ───────────────────────────────────────────────────────────────────

/// Run hybrid retrieval for `query` and return the ranked connected UIDs.
///
/// Reuses [`build_brain_context_hybrid_with_aliases`] — the exact path the
/// product ships — and extracts `connected` node UIDs in their ranked order.
/// Seeds are excluded (they are the query's own anchors, not retrieved
/// results), matching how the metrics judge what retrieval *surfaced*.
fn ranked_uids_for_query(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    config: &HybridSearchConfig,
    aliases: &HashMap<String, Vec<String>>,
    db_path: Option<&Path>,
    do_rerank: bool,
    jq: &JudgedQuery,
) -> Result<Vec<String>, anyhow::Error> {
    let inputs = vec![jq.query.clone()];
    let mut result: BrainContextResult = build_brain_context_hybrid_with_aliases(
        store,
        &inputs,
        tantivy,
        config,
        aliases,
        db_path,
        jq.parsed_intent(),
        None,
        None,
    )?;
    // Feature F17: optionally rerank the leading candidates before scoring, so
    // the harness measures exactly the ordering a `--rerank` caller would get.
    if do_rerank {
        let reranker = select_reranker(db_path);
        rerank(
            &mut result.connected,
            reranker.as_ref(),
            store,
            DEFAULT_TOP_N,
        );
    }
    // nw-159: `seeds` used to be discarded, so a judgment naming the exact UID
    // the query resolves to scored ndcg10 0.0 / mrr 0.0 / p_at_5 0.0 — the
    // symbol WAS retrieved, just as a seed rather than a connected node. Every
    // find-definition-style judgment was structurally unreachable, so the
    // harness under-reported retrieval quality by construction.
    //
    // Seeds rank FIRST: they are the exact matches the query resolved to, so
    // any relevance ordering that placed them below traversal results would be
    // wrong. Deduplicated, because a seed can also appear in `connected`.
    let mut seen = std::collections::HashSet::new();
    Ok(result
        .seeds
        .into_iter()
        .chain(result.connected)
        .map(|n| n.uid)
        .filter(|uid| seen.insert(uid.clone()))
        .collect())
}

/// Run the full evaluation over `queries` and aggregate to an [`EvalReport`].
///
/// For each query the runner executes hybrid retrieval, takes the ranked
/// connected UIDs, and computes nDCG@10, MRR, and precision@5. The aggregate
/// means are simple unweighted averages over the per-query rows.
///
/// `aliases` may be empty (`&HashMap::new()`); `db_path` enables sidecar-backed
/// features (interaction priors, the optional `<db>.rerank.json` model, etc.)
/// and may be `None`. When `do_rerank` is true, Feature F17 reorders the
/// leading candidates of each result before scoring — so the harness measures
/// exactly what a `--rerank` caller ships.
pub fn run_eval(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    queries: &[JudgedQuery],
    config: &HybridSearchConfig,
    aliases: &HashMap<String, Vec<String>>,
    db_path: Option<&Path>,
    do_rerank: bool,
) -> Result<EvalReport, anyhow::Error> {
    let mut per_query = Vec::with_capacity(queries.len());

    for jq in queries {
        // LOW: a query whose seeds don't resolve must not abort the whole
        // run — catch the resolution error per query, warn, score it 0, and
        // continue with the remaining queries.
        let ranked =
            match ranked_uids_for_query(store, tantivy, config, aliases, db_path, do_rerank, jq) {
                Ok(ranked) => ranked,
                Err(e) => {
                    tracing::warn!(
                        query = %jq.query,
                        error = %e,
                        "eval query failed to resolve — scoring it 0 and continuing"
                    );
                    per_query.push(PerQueryRow {
                        query: jq.query.clone(),
                        ndcg10: 0.0,
                        mrr: 0.0,
                        p_at_5: 0.0,
                    });
                    continue;
                }
            };
        // LOW: an unresolvable query scores 0 and the run continues — but say
        // so, otherwise a typo'd seed silently drags the mean down.
        if ranked.is_empty() {
            tracing::warn!(
                query = %jq.query,
                "eval query resolved to zero results — scoring it 0 and continuing"
            );
        }
        per_query.push(PerQueryRow {
            query: jq.query.clone(),
            ndcg10: ndcg_at_k(&ranked, &jq.relevance, 10),
            mrr: mrr(&ranked, &jq.relevance),
            p_at_5: precision_at_k(&ranked, &jq.relevance, 5),
        });
    }

    let n = per_query.len();
    let (mean_ndcg10, mean_mrr, mean_p5) = if n == 0 {
        (0.0, 0.0, 0.0)
    } else {
        let nf = n as f64;
        (
            per_query.iter().map(|r| r.ndcg10).sum::<f64>() / nf,
            per_query.iter().map(|r| r.mrr).sum::<f64>() / nf,
            per_query.iter().map(|r| r.p_at_5).sum::<f64>() / nf,
        )
    };

    Ok(EvalReport {
        per_query,
        mean_ndcg10,
        mean_mrr,
        mean_p5,
        n,
    })
}

// ── Comparison ─────────────────────────────────────────────────────────────

/// Outcome of comparing a "baseline" run against a "treatment" run on the same
/// judged set. Used by `nestweaver eval compare` to judge a quality feature
/// against the >= 5% nDCG@10 gate.
#[derive(Debug, Clone, Serialize)]
pub struct EvalComparison {
    /// A short label for the baseline configuration (e.g. `"prf-off"`).
    pub baseline_label: String,
    /// A short label for the treatment configuration (e.g. `"prf-on"`).
    pub treatment_label: String,
    pub baseline: EvalReport,
    pub treatment: EvalReport,
    /// `treatment.mean_ndcg10 - baseline.mean_ndcg10`.
    pub mean_ndcg10_delta: f64,
    /// Relative change in mean nDCG@10 (`delta / baseline`, `0.0` when the
    /// baseline mean is 0). The >= 5% gate is measured against this.
    pub mean_ndcg10_rel_delta: f64,
    /// Queries where treatment nDCG@10 strictly beat baseline.
    pub wins: usize,
    /// Queries where treatment nDCG@10 was strictly worse than baseline.
    pub losses: usize,
    /// Queries where treatment nDCG@10 equalled baseline (within `1e-9`).
    pub ties: usize,
}

/// Build an [`EvalComparison`] from two already-computed reports over the same
/// query set (same length, same order). Pure aggregation — no retrieval here,
/// so it is unit-testable in isolation.
pub fn compare_reports(
    baseline_label: impl Into<String>,
    baseline: EvalReport,
    treatment_label: impl Into<String>,
    treatment: EvalReport,
) -> EvalComparison {
    let mut wins = 0;
    let mut losses = 0;
    let mut ties = 0;
    for (b, t) in baseline.per_query.iter().zip(treatment.per_query.iter()) {
        let d = t.ndcg10 - b.ndcg10;
        if d > 1e-9 {
            wins += 1;
        } else if d < -1e-9 {
            losses += 1;
        } else {
            ties += 1;
        }
    }

    let mean_ndcg10_delta = treatment.mean_ndcg10 - baseline.mean_ndcg10;
    let mean_ndcg10_rel_delta = if baseline.mean_ndcg10.abs() <= f64::EPSILON {
        0.0
    } else {
        mean_ndcg10_delta / baseline.mean_ndcg10
    };

    EvalComparison {
        baseline_label: baseline_label.into(),
        treatment_label: treatment_label.into(),
        baseline,
        treatment,
        mean_ndcg10_delta,
        mean_ndcg10_rel_delta,
        wins,
        losses,
        ties,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(pairs: &[(&str, u8)]) -> HashMap<String, u8> {
        pairs.iter().map(|(u, g)| ((*u).to_string(), *g)).collect()
    }

    fn ranking(uids: &[&str]) -> Vec<String> {
        uids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ndcg_is_one_for_perfect_ordering() {
        // Grades descending in rank order → perfect → nDCG == 1.0.
        let r = rel(&[("a", 3), ("b", 2), ("c", 1)]);
        let ranked = ranking(&["a", "b", "c"]);
        let score = ndcg_at_k(&ranked, &r, 10);
        assert!(
            (score - 1.0).abs() < 1e-12,
            "perfect ranking should be nDCG=1.0, got {score}"
        );
    }

    #[test]
    fn ndcg_is_less_than_one_for_worse_ordering() {
        let r = rel(&[("a", 3), ("b", 2), ("c", 1)]);
        // Reverse the ideal order → worse than perfect.
        let worse = ranking(&["c", "b", "a"]);
        let score = ndcg_at_k(&worse, &r, 10);
        assert!(score < 1.0, "a worse order must score < 1.0, got {score}");
        assert!(
            score > 0.0,
            "but a relevant doc is still present, got {score}"
        );

        // And the perfect order must beat it.
        let perfect = ndcg_at_k(&ranking(&["a", "b", "c"]), &r, 10);
        assert!(
            perfect > score,
            "perfect ({perfect}) should beat worse ({score})"
        );
    }

    #[test]
    fn ndcg_is_zero_when_no_relevant_docs() {
        let r: HashMap<String, u8> = HashMap::new();
        let ranked = ranking(&["a", "b", "c"]);
        assert_eq!(ndcg_at_k(&ranked, &r, 10), 0.0);

        // All-zero grades → IDCG == 0 → nDCG == 0.
        let all_zero = rel(&[("a", 0), ("b", 0)]);
        assert_eq!(ndcg_at_k(&ranked, &all_zero, 10), 0.0);

        // Empty ranking → 0.
        assert_eq!(ndcg_at_k(&[], &rel(&[("a", 3)]), 10), 0.0);
    }

    #[test]
    fn ndcg_respects_cutoff_k() {
        // A high-grade doc sitting beyond k must not count toward DCG, but it
        // still counts toward IDCG@k? No — IDCG@k is also truncated at k. With
        // k=1 and one relevant doc at rank 0, nDCG@1 == 1.0.
        let r = rel(&[("a", 3)]);
        let ranked = ranking(&["a", "x", "y"]);
        assert!((ndcg_at_k(&ranked, &r, 1) - 1.0).abs() < 1e-12);

        // Relevant doc pushed past the cutoff → DCG@1 = 0 → nDCG@1 = 0.
        let pushed = ranking(&["x", "a"]);
        assert_eq!(ndcg_at_k(&pushed, &r, 1), 0.0);
    }

    #[test]
    fn mrr_is_reciprocal_rank_of_first_relevant() {
        let r = rel(&[("b", 2), ("d", 1)]);
        // First relevant ("b") is at rank 2 (1-based) → 1/2.
        let ranked = ranking(&["a", "b", "c", "d"]);
        assert!((mrr(&ranked, &r) - 0.5).abs() < 1e-12);

        // First relevant at rank 1 → 1.0.
        assert!((mrr(&ranking(&["b", "a"]), &r) - 1.0).abs() < 1e-12);

        // Rank 3 → 1/3.
        let r2 = rel(&[("c", 1)]);
        assert!((mrr(&ranking(&["a", "b", "c"]), &r2) - (1.0 / 3.0)).abs() < 1e-12);
    }

    #[test]
    fn mrr_is_zero_when_no_relevant() {
        let r = rel(&[("z", 3)]);
        assert_eq!(mrr(&ranking(&["a", "b", "c"]), &r), 0.0);
        assert_eq!(mrr(&[], &r), 0.0);
    }

    #[test]
    fn mrr_ignores_grade_zero() {
        // Grade 0 is NOT relevant; first relevant (grade>=1) is "b" at rank 2.
        let r = rel(&[("a", 0), ("b", 1)]);
        assert!((mrr(&ranking(&["a", "b"]), &r) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn precision_at_k_counts_relevant_in_top_k() {
        // 2 of top-4 relevant → 0.5.
        let r = rel(&[("a", 3), ("c", 1)]);
        let ranked = ranking(&["a", "b", "c", "d"]);
        assert!((precision_at_k(&ranked, &r, 4) - 0.5).abs() < 1e-12);

        // top-2: only "a" relevant → 1/2.
        assert!((precision_at_k(&ranked, &r, 2) - 0.5).abs() < 1e-12);

        // top-1: "a" relevant → 1.0.
        assert!((precision_at_k(&ranked, &r, 1) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn precision_at_k_denominator_is_min_k_len() {
        // Ranking shorter than k: denominator is len, not k.
        let r = rel(&[("a", 1), ("b", 1)]);
        let ranked = ranking(&["a", "b"]);
        // Both relevant, k=5, len=2 → 2/2 = 1.0 (not 2/5).
        assert!((precision_at_k(&ranked, &r, 5) - 1.0).abs() < 1e-12);

        // Empty / zero-k → 0.0.
        assert_eq!(precision_at_k(&[], &r, 5), 0.0);
        assert_eq!(precision_at_k(&ranked, &r, 0), 0.0);
    }

    #[test]
    fn compare_reports_counts_wins_losses_ties_and_delta() {
        let mk = |q: &str, ndcg: f64| PerQueryRow {
            query: q.to_string(),
            ndcg10: ndcg,
            mrr: 0.0,
            p_at_5: 0.0,
        };
        let baseline = EvalReport {
            per_query: vec![mk("q1", 0.5), mk("q2", 0.5), mk("q3", 0.5)],
            mean_ndcg10: 0.5,
            mean_mrr: 0.0,
            mean_p5: 0.0,
            n: 3,
        };
        let treatment = EvalReport {
            per_query: vec![mk("q1", 0.8), mk("q2", 0.3), mk("q3", 0.5)],
            mean_ndcg10: (0.8 + 0.3 + 0.5) / 3.0,
            mean_mrr: 0.0,
            mean_p5: 0.0,
            n: 3,
        };
        let cmp = compare_reports("prf-off", baseline, "prf-on", treatment);
        assert_eq!(cmp.wins, 1, "q1 improved");
        assert_eq!(cmp.losses, 1, "q2 regressed");
        assert_eq!(cmp.ties, 1, "q3 unchanged");
        assert!((cmp.mean_ndcg10_delta - ((0.8 + 0.3 + 0.5) / 3.0 - 0.5)).abs() < 1e-12);
        // rel delta = delta / 0.5.
        assert!((cmp.mean_ndcg10_rel_delta - (cmp.mean_ndcg10_delta / 0.5)).abs() < 1e-12);
    }

    #[test]
    fn load_judged_queries_parses_jsonl_and_array() {
        let dir = tempfile::tempdir().unwrap();

        // JSONL form (with a blank line that must be skipped).
        let jsonl = dir.path().join("q.jsonl");
        std::fs::write(
            &jsonl,
            "{\"query\":\"greet\",\"relevance\":{\"sym:a\":3}}\n\n{\"query\":\"hello\",\"intent\":\"find-definition\",\"relevance\":{\"sym:b\":2}}\n",
        )
        .unwrap();
        let qs = load_judged_queries(&jsonl).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].query, "greet");
        assert_eq!(qs[0].relevance.get("sym:a"), Some(&3));
        assert_eq!(
            qs[1].parsed_intent(),
            Some(QueryIntent::FindDefinition),
            "intent string should parse"
        );

        // JSON array form.
        let json = dir.path().join("q.json");
        std::fs::write(&json, "[{\"query\":\"x\",\"relevance\":{\"sym:c\":1}}]").unwrap();
        let qs2 = load_judged_queries(&json).unwrap();
        assert_eq!(qs2.len(), 1);
        assert_eq!(qs2[0].query, "x");
    }

    #[test]
    fn load_judged_queries_errors_on_missing_and_empty() {
        // Missing file → clear error.
        let missing = std::path::Path::new("/nonexistent/eval-queries.jsonl");
        let err = load_judged_queries(missing).unwrap_err().to_string();
        assert!(err.contains("failed to read"), "got: {err}");

        // Empty file → clear error.
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.jsonl");
        std::fs::write(&empty, "   \n\n").unwrap();
        let err2 = load_judged_queries(&empty).unwrap_err().to_string();
        assert!(err2.contains("empty"), "got: {err2}");
    }

    #[test]
    fn run_eval_scores_a_hand_labeled_query_on_an_indexed_store() {
        use crate::index::index_directory_in_memory;
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            r#"
function greet(name) { return hello(name); }
function hello(name) { return "Hello " + name; }
"#,
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        // Find the UID of `hello`, which `greet` calls — so seeding on "greet"
        // should surface `hello` as a connected node with high relevance.
        let hello = store
            .search_symbols_by_name(
                "hello",
                5,
                &nestweaver_store::SeedResolutionConfig::default(),
            )
            .unwrap()
            .into_iter()
            .next()
            .expect("hello symbol should be indexed");

        let jq = JudgedQuery {
            query: "greet".to_string(),
            intent: None,
            relevance: {
                let mut m = HashMap::new();
                m.insert(hello.uid.clone(), 3u8);
                m
            },
        };

        let cfg = HybridSearchConfig::default();
        let aliases: HashMap<String, Vec<String>> = HashMap::new();
        let report = run_eval(&store, None, &[jq], &cfg, &aliases, None, false).unwrap();

        assert_eq!(report.n, 1);
        assert_eq!(report.per_query.len(), 1);
        // `hello` is reachable from `greet` via a CALLS edge, so PPR should
        // surface it among the connected nodes → all three metrics > 0.
        assert!(
            report.mean_ndcg10 > 0.0,
            "expected hello to be retrieved for seed 'greet'; ndcg={}",
            report.mean_ndcg10
        );
        assert!(report.mean_mrr > 0.0, "mrr={}", report.mean_mrr);
        assert!(report.mean_p5 > 0.0, "p5={}", report.mean_p5);
        // Means equal the single row.
        assert!((report.mean_ndcg10 - report.per_query[0].ndcg10).abs() < 1e-12);
    }

    /// LOW: a query whose seeds don't resolve must not abort the whole eval
    /// run — it is scored 0 (with a warning) and the remaining queries are
    /// still evaluated. Regression test: `run_eval` used to propagate the
    /// seed-resolution error via `?`, failing the entire report.
    /// nw-159 (secondary): a mistyped judgment key used to be swallowed by
    /// `serde(default)`, leaving `relevance` empty — and the harness then
    /// printed a confident 0.0 for a query whose judgments it had silently
    /// discarded.
    #[test]
    fn a_mistyped_judgment_key_fails_to_parse_instead_of_scoring_zero() {
        let good: JudgedQuery =
            serde_json::from_str(r#"{"query":"rollback_current","relevance":{"sym:a":3}}"#)
                .expect("a well-formed judgment must parse");
        assert_eq!(good.relevance.get("sym:a"), Some(&3));

        let error = serde_json::from_str::<JudgedQuery>(
            r#"{"query":"rollback_current","relevant":{"sym:a":3}}"#,
        )
        .expect_err("a mistyped key must be rejected, not silently ignored");
        assert!(
            error.to_string().contains("relevant"),
            "the error must name the offending key: {error}"
        );
    }

    #[test]
    fn run_eval_scores_unresolvable_query_zero_and_continues() {
        use crate::index::index_directory_in_memory;
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(name) { return name; }\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let mk = |q: &str| JudgedQuery {
            query: q.to_string(),
            intent: None,
            relevance: HashMap::new(),
        };
        let bad = mk("definitely-not-a-real-seed-zzz");
        let good = mk("greet");

        let cfg = HybridSearchConfig::default();
        let aliases: HashMap<String, Vec<String>> = HashMap::new();
        let report = run_eval(&store, None, &[bad, good], &cfg, &aliases, None, false)
            .expect("an unresolvable query must not abort the whole eval run");

        assert_eq!(report.n, 2, "both queries must be scored");
        assert_eq!(report.per_query[0].query, "definitely-not-a-real-seed-zzz");
        assert_eq!(report.per_query[0].ndcg10, 0.0);
        assert_eq!(report.per_query[0].mrr, 0.0);
        assert_eq!(report.per_query[0].p_at_5, 0.0);
        assert_eq!(report.per_query[1].query, "greet");
    }
}
