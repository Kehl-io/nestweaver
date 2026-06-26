//! Reciprocal Rank Fusion (RRF) merge with weighted local preference.
//!
//! Merges local and server result lists using RRF (k=60). Local results
//! receive a 1.5x weight multiplier to prevent server-dominant tails when
//! result sets are asymmetric (spec validation finding #8).

use crate::dedup::{
    extract_identity, Confidence, MergedResult, Provenance, SymbolIdentity,
};
use std::collections::HashMap;

/// RRF smoothing constant. Standard value from the original RRF paper.
const RRF_K: f64 = 60.0;

/// Weight multiplier for local results. Compensates for asymmetric
/// result set sizes — a server indexing 50 repos will naturally return
/// more results than a local daemon indexing 3.
const LOCAL_WEIGHT: f64 = 1.5;

/// Weight for server results (baseline).
const SERVER_WEIGHT: f64 = 1.0;

/// Merge local and server results using weighted RRF (k=60).
///
/// Local results get a 1.5x multiplier to compensate for asymmetric
/// result set sizes. This ensures local results are not drowned by a
/// server returning 5x more results.
///
/// Deduplication uses [`extract_identity`] (scope-hash identity tuple).
/// When both sources have the same symbol, local wins on content and
/// scores are accumulated.
pub fn rrf_merge(
    local_results: Vec<serde_json::Value>,
    server_results: Vec<serde_json::Value>,
) -> Vec<MergedResult> {
    rrf_merge_weighted(local_results, server_results, RRF_K, LOCAL_WEIGHT)
}

/// RRF merge with configurable k and local weight (for testing).
pub fn rrf_merge_weighted(
    local_results: Vec<serde_json::Value>,
    server_results: Vec<serde_json::Value>,
    k: f64,
    local_weight: f64,
) -> Vec<MergedResult> {
    let mut scored: HashMap<SymbolIdentity, MergedResult> = HashMap::new();
    let mut unkeyed: Vec<MergedResult> = Vec::new();

    // Score local results with weighted RRF
    for (rank, val) in local_results.into_iter().enumerate() {
        let rrf_score = local_weight / (rank as f64 + k + 1.0);
        let confidence = infer_confidence(&val);
        match extract_identity(&val) {
            Some(id) => {
                scored.insert(
                    id,
                    MergedResult {
                        value: val,
                        provenance: Provenance::Local,
                        confidence,
                        score: rrf_score,
                    },
                );
            }
            None => {
                unkeyed.push(MergedResult {
                    value: val,
                    provenance: Provenance::Local,
                    confidence: Confidence::Heuristic,
                    score: rrf_score,
                });
            }
        }
    }

    // Score server results with baseline weight, merge with local
    for (rank, val) in server_results.into_iter().enumerate() {
        let rrf_score = SERVER_WEIGHT / (rank as f64 + k + 1.0);
        let confidence = infer_confidence(&val);
        match extract_identity(&val) {
            Some(id) => {
                if let Some(existing) = scored.get_mut(&id) {
                    // Duplicate: local wins on content, accumulate score
                    existing.provenance = Provenance::Both;
                    existing.score += rrf_score;
                } else {
                    scored.insert(
                        id,
                        MergedResult {
                            value: val,
                            provenance: Provenance::Server,
                            confidence,
                            score: rrf_score,
                        },
                    );
                }
            }
            None => {
                unkeyed.push(MergedResult {
                    value: val,
                    provenance: Provenance::Server,
                    confidence: Confidence::Heuristic,
                    score: rrf_score,
                });
            }
        }
    }

    let mut results: Vec<_> = scored.into_values().chain(unkeyed).collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// Infer confidence from a result's structural markers.
fn infer_confidence(result: &serde_json::Value) -> Confidence {
    let has_scope = result
        .get("scope_chain")
        .or_else(|| result.get("scope"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if has_scope {
        Confidence::Precise
    } else {
        Confidence::Heuristic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_result(repo: &str, name: &str, scope: &str) -> serde_json::Value {
        json!({
            "repo_url": repo,
            "file_path": "src/lib.rs",
            "symbol_name": name,
            "scope_chain": scope,
        })
    }

    #[test]
    fn local_results_rank_higher_with_weight() {
        // Same rank (0) in both lists, different symbols
        let local = vec![make_result("repo", "sym_a", "mod::a")];
        let server = vec![make_result("repo", "sym_b", "mod::b")];

        let merged = rrf_merge(local, server);
        assert_eq!(merged.len(), 2);
        // Local sym_a should rank above server sym_b because of 1.5x weight
        assert_eq!(merged[0].value["symbol_name"], "sym_a");
        assert_eq!(merged[0].provenance, Provenance::Local);
    }

    #[test]
    fn duplicate_symbol_gets_combined_score() {
        let local = vec![make_result("repo", "sym_a", "mod::a")];
        let server = vec![make_result("repo", "sym_a", "mod::a")];

        let merged = rrf_merge(local, server);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].provenance, Provenance::Both);
        // Combined score > either individual score
        let single_local = LOCAL_WEIGHT / (0.0 + RRF_K + 1.0);
        assert!(merged[0].score > single_local);
    }

    #[test]
    fn duplicate_local_wins_on_content() {
        let local = vec![json!({
            "repo_url": "repo",
            "file_path": "src/lib.rs",
            "symbol_name": "sym_a",
            "scope_chain": "mod::a",
            "body": "local code"
        })];
        let server = vec![json!({
            "repo_url": "repo",
            "file_path": "src/lib.rs",
            "symbol_name": "sym_a",
            "scope_chain": "mod::a",
            "body": "server code"
        })];

        let merged = rrf_merge(local, server);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value["body"], "local code");
    }

    #[test]
    fn asymmetric_sets_local_not_drowned() {
        // 2 local results vs 20 server results
        let local: Vec<_> = (0..2)
            .map(|i| make_result("repo", &format!("local_{i}"), &format!("mod::local_{i}")))
            .collect();
        let server: Vec<_> = (0..20)
            .map(|i| make_result("repo", &format!("server_{i}"), &format!("mod::server_{i}")))
            .collect();

        let merged = rrf_merge(local, server);
        // First result should be local (1.5x weight at rank 0)
        assert_eq!(merged[0].provenance, Provenance::Local);
        // Both local results should be in the top 5
        let local_in_top5 = merged[..5]
            .iter()
            .filter(|r| r.provenance == Provenance::Local)
            .count();
        assert!(local_in_top5 >= 2, "expected both local results in top 5, got {local_in_top5}");
    }

    #[test]
    fn empty_local_returns_server_only() {
        let server = vec![make_result("repo", "sym_a", "mod::a")];
        let merged = rrf_merge(vec![], server);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].provenance, Provenance::Server);
    }

    #[test]
    fn empty_server_returns_local_only() {
        let local = vec![make_result("repo", "sym_a", "mod::a")];
        let merged = rrf_merge(local, vec![]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].provenance, Provenance::Local);
    }

    #[test]
    fn both_empty_returns_empty() {
        let merged = rrf_merge(vec![], vec![]);
        assert!(merged.is_empty());
    }

    #[test]
    fn rrf_scores_decrease_with_rank() {
        let results: Vec<_> = (0..5)
            .map(|i| make_result("repo", &format!("sym_{i}"), &format!("mod::{i}")))
            .collect();

        let merged = rrf_merge(results, vec![]);
        for window in merged.windows(2) {
            assert!(
                window[0].score >= window[1].score,
                "scores should be non-increasing"
            );
        }
    }

    #[test]
    fn configurable_k_affects_scores() {
        let local = vec![make_result("repo", "sym_a", "mod::a")];

        let merged_low_k = rrf_merge_weighted(local.clone(), vec![], 10.0, 1.5);
        let merged_high_k = rrf_merge_weighted(local, vec![], 100.0, 1.5);

        // Lower k = higher score for top-ranked results
        assert!(merged_low_k[0].score > merged_high_k[0].score);
    }

    #[test]
    fn configurable_weight_affects_ranking() {
        let local = vec![make_result("repo", "local_sym", "mod::local")];
        let server = vec![make_result("repo", "server_sym", "mod::server")];

        // With high local weight, local should dominate
        let merged_high = rrf_merge_weighted(local.clone(), server.clone(), 60.0, 10.0);
        assert_eq!(merged_high[0].value["symbol_name"], "local_sym");

        // With weight 1.0 (equal), both are at rank 0 so equal score — order is
        // nondeterministic, but both should be present
        let merged_equal = rrf_merge_weighted(local, server, 60.0, 1.0);
        assert_eq!(merged_equal.len(), 2);
    }

    #[test]
    fn unkeyed_results_preserved() {
        // Results missing identity fields should still appear
        let local = vec![json!({"text": "local note"})];
        let server = vec![json!({"text": "server note"})];

        let merged = rrf_merge(local, server);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn confidence_precise_when_scope_present() {
        let local = vec![make_result("repo", "sym", "mod::cls")];
        let merged = rrf_merge(local, vec![]);
        assert_eq!(merged[0].confidence, Confidence::Precise);
    }

    #[test]
    fn confidence_heuristic_when_no_scope() {
        let local = vec![json!({
            "repo_url": "repo",
            "file_path": "src/lib.rs",
            "symbol_name": "sym",
        })];
        let merged = rrf_merge(local, vec![]);
        assert_eq!(merged[0].confidence, Confidence::Heuristic);
    }
}
