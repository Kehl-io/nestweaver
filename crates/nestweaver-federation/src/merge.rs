//! Reciprocal Rank Fusion (RRF) merge with weighted local preference.
//!
//! Merges local and server result lists using RRF (k=60). Local results
//! receive a 1.5x weight multiplier to prevent server-dominant tails when
//! result sets are asymmetric (spec validation finding #8).

use crate::dedup::{
    Confidence, MergedResult, Provenance, SymbolIdentity, assign_confidence, extract_identity,
};
use std::collections::{HashMap, HashSet};

/// RRF smoothing constant. Standard value from the original RRF paper.
const RRF_K: f64 = 60.0;

/// Weight multiplier for local results. Compensates for asymmetric
/// result set sizes — a server indexing 50 repos will naturally return
/// more results than a local daemon indexing 3.
const LOCAL_WEIGHT: f64 = 1.5;

/// Weight for server results (baseline).
const SERVER_WEIGHT: f64 = 1.0;

/// Canonical, instance-independent string form of a [`SymbolIdentity`], used as
/// the stable secondary sort key so equal-score ties break deterministically
/// (Elasticsearch #101232 — score ties must not fall back to hash-map order).
///
/// Every component is instance-invariant: `repo_url` is the normalized repo key
/// (`extract_identity`), and `scope_hash` is a fixed-key `DefaultHasher` digest,
/// so this key is identical across processes and runs.
fn identity_tiebreaker(id: &SymbolIdentity) -> String {
    // \u{1f} (unit separator) can't appear in the components, so this is an
    // injective encoding — distinct identities never collide.
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        id.repo_url, id.file_path, id.symbol_name, id.scope_hash
    )
}

/// Sort merged results by `(score desc, tiebreaker asc)` for total determinism.
/// `keyed` carries each result's identity tiebreaker; `unkeyed` results (no
/// identity) fall back to their serialized value and sort after keyed ties.
fn finalize_ordering(
    keyed: HashMap<SymbolIdentity, MergedResult>,
    unkeyed: Vec<MergedResult>,
) -> Vec<MergedResult> {
    let mut all: Vec<(String, MergedResult)> = keyed
        .into_iter()
        .map(|(id, r)| (identity_tiebreaker(&id), r))
        .collect();
    // Prefix unkeyed keys with a high separator so keyed results win ties
    // against unkeyed ones deterministically; the value string keeps unkeyed
    // ordering stable among themselves.
    for r in unkeyed {
        let tb = format!("\u{7f}{}", r.value);
        all.push((tb, r));
    }
    all.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    all.into_iter().map(|(_, r)| r).collect()
}

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

    // Score local results with weighted RRF. First source to key an identity
    // owns its content; a repeated identity accumulates its RRF contribution.
    for (rank, val) in local_results.into_iter().enumerate() {
        let rrf_score = local_weight / (rank as f64 + k + 1.0);
        let confidence = infer_confidence(&val);
        match extract_identity(&val) {
            Some(id) => match scored.get_mut(&id) {
                Some(existing) => existing.score += rrf_score,
                None => {
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
            },
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

    // Score server results with baseline weight, accumulating into local.
    for (rank, val) in server_results.into_iter().enumerate() {
        let rrf_score = SERVER_WEIGHT / (rank as f64 + k + 1.0);
        let confidence = infer_confidence(&val);
        match extract_identity(&val) {
            Some(id) => match scored.get_mut(&id) {
                Some(existing) => {
                    // Duplicate: local wins on content, accumulate score.
                    // Only escalate to Both when the row came from local — a
                    // server-internal repeat stays Server.
                    if existing.provenance == Provenance::Local {
                        existing.provenance = Provenance::Both;
                    }
                    existing.score += rrf_score;
                }
                None => {
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
            },
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

    finalize_ordering(scored, unkeyed)
}

/// Merge with awareness of locally modified files for staleness labeling.
///
/// Results from the server that touch files in `locally_modified_files`
/// are tagged `Confidence::Stale` instead of `Precise`.
pub fn rrf_merge_with_modified(
    local_results: Vec<serde_json::Value>,
    server_results: Vec<serde_json::Value>,
    locally_modified_files: &HashSet<String>,
) -> Vec<MergedResult> {
    let mut scored: HashMap<SymbolIdentity, MergedResult> = HashMap::new();
    let mut unkeyed: Vec<MergedResult> = Vec::new();

    for (rank, val) in local_results.into_iter().enumerate() {
        let rrf_score = LOCAL_WEIGHT / (rank as f64 + RRF_K + 1.0);
        let confidence = assign_confidence(&val, Provenance::Local, locally_modified_files);
        match extract_identity(&val) {
            Some(id) => match scored.get_mut(&id) {
                Some(existing) => existing.score += rrf_score,
                None => {
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
            },
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

    for (rank, val) in server_results.into_iter().enumerate() {
        let rrf_score = SERVER_WEIGHT / (rank as f64 + RRF_K + 1.0);
        let confidence = assign_confidence(&val, Provenance::Server, locally_modified_files);
        match extract_identity(&val) {
            Some(id) => match scored.get_mut(&id) {
                Some(existing) => {
                    if existing.provenance == Provenance::Local {
                        existing.provenance = Provenance::Both;
                    }
                    existing.score += rrf_score;
                    // If server version is stale, downgrade confidence.
                    if confidence == Confidence::Stale {
                        existing.confidence = Confidence::Stale;
                    }
                }
                None => {
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
            },
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

    finalize_ordering(scored, unkeyed)
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
    fn rrf_accumulates_duplicate_across_lists() {
        // Canonical RRF (Cormack et al. 2009): score(d) = Σ_i 1/(k + rank_i(d)).
        // A doc that appears MULTIPLE times must accumulate every contribution,
        // never overwrite. Here sym_a appears twice in the local list (ranks 0,1)
        // and once in the server list (rank 0), so its score must be the SUM of
        // all three contributions.
        let local = vec![
            make_result("repo", "sym_a", "mod::a"),
            make_result("repo", "sym_a", "mod::a"),
        ];
        let server = vec![make_result("repo", "sym_a", "mod::a")];

        let merged = rrf_merge(local, server);
        assert_eq!(merged.len(), 1, "same identity must collapse to one row");
        assert_eq!(merged[0].provenance, Provenance::Both);

        let expected = LOCAL_WEIGHT / (0.0 + RRF_K + 1.0)   // local rank 0
            + LOCAL_WEIGHT / (1.0 + RRF_K + 1.0)            // local rank 1
            + SERVER_WEIGHT / (0.0 + RRF_K + 1.0); // server rank 0
        assert!(
            (merged[0].score - expected).abs() < 1e-12,
            "score must accumulate all contributions: got {}, want {}",
            merged[0].score,
            expected
        );
    }

    #[test]
    fn rrf_merge_is_deterministic() {
        // Two distinct symbols with EQUAL RRF scores (weight 1.0, both at rank 0).
        // With only a score-based sort, HashMap iteration order breaks the tie
        // differently across runs. A stable identity tiebreaker must pin the
        // order so every run is byte-identical.
        let local = vec![make_result("repo", "sym_zzz", "mod::z")];
        let server = vec![make_result("repo", "sym_aaa", "mod::a")];

        let mut orderings = std::collections::HashSet::new();
        for _ in 0..64 {
            let merged = rrf_merge_weighted(local.clone(), server.clone(), 60.0, 1.0);
            assert_eq!(merged.len(), 2);
            // Scores must be exactly equal for this to test tie determinism.
            assert!((merged[0].score - merged[1].score).abs() < 1e-12);
            let order: Vec<String> = merged
                .iter()
                .map(|r| r.value["symbol_name"].as_str().unwrap().to_string())
                .collect();
            orderings.insert(order);
        }
        assert_eq!(
            orderings.len(),
            1,
            "tie ordering must be stable across runs, saw {} distinct orderings",
            orderings.len()
        );
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
        assert!(
            local_in_top5 >= 2,
            "expected both local results in top 5, got {local_in_top5}"
        );
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

        // With weight 1.0 (equal), both are at rank 0 so equal score — the tie
        // is now broken deterministically by identity, and both are present.
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

    // ── rrf_merge_with_modified tests ────────────────────────────

    #[test]
    fn server_result_stale_when_file_modified() {
        let server = vec![json!({
            "repo_url": "repo",
            "file_path": "src/lib.rs",
            "symbol_name": "func",
            "scope_chain": "mod::func",
        })];
        let modified = HashSet::from(["src/lib.rs".to_string()]);
        let merged = rrf_merge_with_modified(vec![], server, &modified);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].confidence, Confidence::Stale);
    }

    #[test]
    fn server_result_precise_when_file_not_modified() {
        let server = vec![json!({
            "repo_url": "repo",
            "file_path": "src/other.rs",
            "symbol_name": "func",
            "scope_chain": "mod::func",
        })];
        let modified = HashSet::from(["src/lib.rs".to_string()]);
        let merged = rrf_merge_with_modified(vec![], server, &modified);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].confidence, Confidence::Precise);
    }

    #[test]
    fn local_result_precise_even_when_file_modified() {
        let local = vec![json!({
            "repo_url": "repo",
            "file_path": "src/lib.rs",
            "symbol_name": "func",
            "scope_chain": "mod::func",
        })];
        let modified = HashSet::from(["src/lib.rs".to_string()]);
        let merged = rrf_merge_with_modified(local, vec![], &modified);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].confidence, Confidence::Precise);
    }

    #[test]
    fn duplicate_downgraded_to_stale_when_server_file_modified() {
        let local = vec![json!({
            "repo_url": "repo",
            "file_path": "src/lib.rs",
            "symbol_name": "func",
            "scope_chain": "mod::func",
            "body": "local version",
        })];
        let server = vec![json!({
            "repo_url": "repo",
            "file_path": "src/lib.rs",
            "symbol_name": "func",
            "scope_chain": "mod::func",
            "body": "server version",
        })];
        let modified = HashSet::from(["src/lib.rs".to_string()]);
        let merged = rrf_merge_with_modified(local, server, &modified);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].provenance, Provenance::Both);
        // Should be downgraded to stale because the server's version is outdated.
        assert_eq!(merged[0].confidence, Confidence::Stale);
        // Local content wins.
        assert_eq!(merged[0].value["body"], "local version");
    }

    #[test]
    fn no_modified_files_keeps_precise() {
        let local = vec![make_result("repo", "a", "mod::a")];
        let server = vec![make_result("repo", "b", "mod::b")];
        let merged = rrf_merge_with_modified(local, server, &HashSet::new());
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|r| r.confidence == Confidence::Precise));
    }
}
