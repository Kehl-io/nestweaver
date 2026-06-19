//! Integration test: verify that weighted_score_fuse produces
//! correct rankings when combining PPR, BM25, and semantic signals.

use nestweaver_engine::query::HybridSearchConfig;

#[test]
fn test_semantic_leg_improves_nl_query() {
    // Scenario: user queries "how does authentication work"
    // PPR found nothing (no seed resolved)
    let ppr: Vec<(String, f64)> = vec![];

    // BM25 found a note with "authentication" in the title
    let bm25 = vec![nestweaver_store::SearchHit {
        uid: "note:auth-guide".to_string(),
        kind: "note".to_string(),
        title: "Authentication Guide".to_string(),
        vault_uid: "v1".to_string(),
        score: 12.5,
    }];

    // Semantic search found the actual auth middleware symbol
    let semantic = vec![
        ("sym:auth-validate".to_string(), 0.87),
        ("note:auth-guide".to_string(), 0.82),
        ("head:auth-config".to_string(), 0.75),
    ];

    let config = HybridSearchConfig::default();
    let results = nestweaver_engine::query::weighted_score_fuse(
        &ppr,
        &bm25,
        &semantic,
        config.weight_ppr,
        config.weight_bm25,
        config.weight_semantic,
    );

    // With empty PPR, semantic results should dominate
    assert!(results.len() >= 3);
    let top_uids: Vec<&str> = results.iter().take(3).map(|(u, _)| u.as_str()).collect();
    assert!(top_uids.contains(&"sym:auth-validate"));
    assert!(top_uids.contains(&"note:auth-guide"));
}
