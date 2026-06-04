// PR blast radius analysis: maps changed files to affected symbols,
// runs transitive impact analysis, groups by cluster, and scores risk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::GraphStore;

use crate::cluster_dispatch::{ClusteringOutput, load_clusters};
use crate::process::RiskLevel;

/// A symbol that was directly changed (lives in a changed file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedSymbol {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub kind: String,
    pub pagerank_score: Option<f64>,
}

/// A symbol transitively affected by a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedSymbol {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub kind: String,
    pub depth: u32,
    pub edge_type: String,
    pub confidence: f32,
    /// Confidence-weighted impact score (1.0 = direct high-confidence edge,
    /// decays multiplicatively through the graph). Used for sorting results
    /// so the most-affected symbols appear first.
    pub impact_score: f64,
}

/// A cluster (community) that contains affected symbols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedCluster {
    pub id: u32,
    pub name: String,
    pub affected_count: usize,
    pub total_count: usize,
    pub cohesion: f64,
}

/// Full result of a blast radius analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlastRadiusResult {
    pub changed_symbols: Vec<ChangedSymbol>,
    pub affected_symbols: Vec<AffectedSymbol>,
    pub affected_clusters: Vec<AffectedCluster>,
    pub risk_level: RiskLevel,
    pub summary: String,
}

/// Analyze the blast radius of a set of changed files.
///
/// 1. Maps changed files to their symbols in the graph.
/// 2. For each symbol, runs transitive impact analysis (CALLS, IMPORTS,
///    EXTENDS, IMPLEMENTS edges) up to `max_depth`.
/// 3. Groups affected symbols by cluster/community (if cluster data exists).
/// 4. Scores risk based on: number of affected symbols, PageRank centrality
///    of changed symbols, and number of clusters touched.
///
/// Risk levels:
/// - Low: <10 affected symbols
/// - Medium: 10-50 affected symbols
/// - High: 50-200 affected symbols
/// - Critical: >200 affected symbols (mapped to High since RiskLevel has 3 variants)
pub fn analyze_blast_radius(
    store: &GraphStore,
    changed_files: &[PathBuf],
    max_depth: u32,
    db_path: Option<&Path>,
) -> Result<BlastRadiusResult> {
    // Step 1: Map changed files to symbols.
    let mut changed_symbols: Vec<ChangedSymbol> = Vec::new();
    let mut changed_uids: HashSet<String> = HashSet::new();

    for file in changed_files {
        let file_str = file.to_string_lossy();
        let syms = store.symbols_in_file(&file_str).unwrap_or_default();
        for sym in syms {
            if changed_uids.insert(sym.uid.clone()) {
                changed_symbols.push(ChangedSymbol {
                    uid: sym.uid.clone(),
                    name: sym.name.clone(),
                    file_path: sym.file_path.clone(),
                    kind: sym.kind.to_string(),
                    pagerank_score: sym.pagerank_score,
                });
            }
        }
    }

    // Step 2: For each changed symbol, run transitive impact analysis.
    let mut affected_symbols: Vec<AffectedSymbol> = Vec::new();
    let mut affected_uids: HashSet<String> = HashSet::new();

    for cs in &changed_symbols {
        let impact_nodes = store.impact(&cs.uid, max_depth, 0.0).unwrap_or_default();
        for node in impact_nodes {
            // Skip symbols that are themselves in the changed set.
            if changed_uids.contains(&node.uid) {
                continue;
            }
            if affected_uids.insert(node.uid.clone()) {
                // Look up the symbol's kind from the store.
                let kind = store
                    .lookup_symbol(&node.uid)
                    .map(|s| s.kind.to_string())
                    .unwrap_or_default();
                affected_symbols.push(AffectedSymbol {
                    uid: node.uid,
                    name: node.name,
                    file_path: node.file_path,
                    kind,
                    depth: node.depth,
                    edge_type: node.edge_type,
                    confidence: node.confidence,
                    impact_score: node.impact_score,
                });
            }
        }
    }

    // Sort affected symbols by impact_score (highest first).
    affected_symbols.sort_by(|a, b| {
        b.impact_score
            .partial_cmp(&a.impact_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.uid.cmp(&b.uid))
    });

    // Step 3: Group by clusters if cluster data is available.
    let mut affected_clusters: Vec<AffectedCluster> = Vec::new();
    let all_affected_uids: HashSet<&str> = changed_uids
        .iter()
        .chain(affected_uids.iter())
        .map(|s| s.as_str())
        .collect();

    if let Some(db) = db_path
        && let Ok(Some(clustering)) = load_clusters(db)
    {
        affected_clusters = compute_affected_clusters(&clustering, &all_affected_uids);
    }

    // Step 4: Score risk.
    let total_affected = affected_symbols.len();
    let clusters_touched = affected_clusters.len();

    // Factor in PageRank centrality: if high-centrality symbols are changed,
    // bump the risk. Average PageRank of changed symbols; a score > 0.01
    // is considered "high centrality" in a typical graph.
    let avg_pagerank = if changed_symbols.is_empty() {
        0.0
    } else {
        let sum: f64 = changed_symbols
            .iter()
            .filter_map(|s| s.pagerank_score)
            .sum();
        let count = changed_symbols
            .iter()
            .filter(|s| s.pagerank_score.is_some())
            .count();
        if count > 0 { sum / count as f64 } else { 0.0 }
    };

    let risk_level = compute_risk_level(total_affected, clusters_touched, avg_pagerank);

    let summary = format!(
        "{} changed symbol(s) in {} file(s), {} transitively affected symbol(s), \
         {} cluster(s) touched. Risk: {:?}.",
        changed_symbols.len(),
        changed_files.len(),
        affected_symbols.len(),
        clusters_touched,
        risk_level,
    );

    Ok(BlastRadiusResult {
        changed_symbols,
        affected_symbols,
        affected_clusters,
        risk_level,
        summary,
    })
}

/// Compute risk level based on affected count, clusters, and centrality.
fn compute_risk_level(
    affected_count: usize,
    clusters_touched: usize,
    avg_pagerank: f64,
) -> RiskLevel {
    // Base risk from affected symbol count:
    //   <10 = Low (0), 10-50 = Medium (1), 50-200 = High (2), >200 = High (3)
    let base = match affected_count {
        0..10 => 0,
        10..50 => 1,
        50..200 => 2,
        _ => 3,
    };

    // Boost for high-centrality symbols (avg PageRank > 0.01).
    let centrality_boost = if avg_pagerank > 0.01 { 1 } else { 0 };

    // Boost for touching many clusters (>3 clusters).
    let cluster_boost = if clusters_touched > 3 { 1 } else { 0 };

    let score = base + centrality_boost + cluster_boost;

    match score {
        0 => RiskLevel::Low,
        1 => RiskLevel::Medium,
        _ => RiskLevel::High,
    }
}

/// Determine which clusters are affected by the changed + transitively affected symbols.
fn compute_affected_clusters(
    clustering: &ClusteringOutput,
    affected_uids: &HashSet<&str>,
) -> Vec<AffectedCluster> {
    let mut result = Vec::new();
    for community in &clustering.communities {
        let affected_count = community
            .members
            .iter()
            .filter(|m| affected_uids.contains(m.uid.as_str()))
            .count();
        if affected_count > 0 {
            result.push(AffectedCluster {
                id: community.id,
                name: community.name.clone(),
                affected_count,
                total_count: community.member_count,
                cohesion: community.cohesion,
            });
        }
    }
    // Sort by affected count descending.
    result.sort_by_key(|c| std::cmp::Reverse(c.affected_count));
    result
}

/// Get changed files from `git diff --name-only` in the given repo path.
///
/// When `base_ref` is provided, diffs against that ref. Otherwise diffs
/// against HEAD (showing unstaged + staged changes).
pub fn changed_files_from_git(repo_path: &Path, base_ref: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new("git");
    cmd.arg("diff").arg("--name-only");

    if let Some(base) = base_ref {
        cmd.arg(base);
    }

    let output = cmd
        .current_dir(repo_path)
        .output()
        .context("failed to run git diff --name-only")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<PathBuf> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect();

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_dispatch::CommunityInfo;

    #[test]
    fn compute_risk_level_low() {
        assert_eq!(compute_risk_level(5, 1, 0.001), RiskLevel::Low);
    }

    #[test]
    fn compute_risk_level_medium_by_count() {
        assert_eq!(compute_risk_level(25, 1, 0.001), RiskLevel::Medium);
    }

    #[test]
    fn compute_risk_level_high_by_count() {
        assert_eq!(compute_risk_level(100, 1, 0.001), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_critical_by_count() {
        assert_eq!(compute_risk_level(300, 1, 0.001), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_boosted_by_centrality() {
        // 25 affected would be Medium, but high centrality bumps it to High
        assert_eq!(compute_risk_level(25, 1, 0.05), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_boosted_by_clusters() {
        // 25 affected would be Medium, but >3 clusters bumps it to High
        assert_eq!(compute_risk_level(25, 5, 0.001), RiskLevel::High);
    }

    #[test]
    fn compute_affected_clusters_filters_empty() {
        let clustering = ClusteringOutput {
            resolution: 1.0,
            modularity: 0.5,
            communities: vec![
                CommunityInfo {
                    id: 0,
                    name: "cluster-0".to_string(),
                    cohesion: 0.8,
                    member_count: 2,
                    members: vec![
                        crate::cluster_dispatch::ClusterMember {
                            uid: "sym:a".to_string(),
                            name: "a".to_string(),
                            file_path: "a.rs".to_string(),
                            kind: "Function".to_string(),
                        },
                        crate::cluster_dispatch::ClusterMember {
                            uid: "sym:b".to_string(),
                            name: "b".to_string(),
                            file_path: "b.rs".to_string(),
                            kind: "Function".to_string(),
                        },
                    ],
                    key_files: vec!["a.rs".to_string()],
                },
                CommunityInfo {
                    id: 1,
                    name: "cluster-1".to_string(),
                    cohesion: 0.6,
                    member_count: 1,
                    members: vec![crate::cluster_dispatch::ClusterMember {
                        uid: "sym:c".to_string(),
                        name: "c".to_string(),
                        file_path: "c.rs".to_string(),
                        kind: "Function".to_string(),
                    }],
                    key_files: vec!["c.rs".to_string()],
                },
            ],
        };

        let affected: HashSet<&str> = ["sym:a"].into_iter().collect();
        let clusters = compute_affected_clusters(&clustering, &affected);

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].id, 0);
        assert_eq!(clusters[0].affected_count, 1);
        assert_eq!(clusters[0].total_count, 2);
    }

    #[test]
    fn analyze_blast_radius_empty_store() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let result =
            analyze_blast_radius(&store, &[PathBuf::from("nonexistent.rs")], 3, None).unwrap();
        assert!(result.changed_symbols.is_empty());
        assert!(result.affected_symbols.is_empty());
        assert_eq!(result.risk_level, RiskLevel::Low);
    }

    #[test]
    fn analyze_blast_radius_with_symbols() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        let sym_a = Symbol {
            uid: "sym:a".to_string(),
            name: "fn_a".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/a.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn fn_a()".to_string(),
            summary: None,
            content_hash: "h1".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };
        let sym_b = Symbol {
            uid: "sym:b".to_string(),
            name: "fn_b".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/b.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn fn_b()".to_string(),
            summary: None,
            content_hash: "h2".to_string(),
            embedding: None,
            pagerank_score: Some(0.1),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };

        store.insert_symbol(&sym_a).expect("insert sym_a");
        store.insert_symbol(&sym_b).expect("insert sym_b");

        // sym_b calls sym_a, so changing a.rs should show sym_b as affected
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:b".to_string(),
                target_uid: "sym:a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .expect("insert edge");

        let result = analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], 3, None).unwrap();

        assert_eq!(result.changed_symbols.len(), 1);
        assert_eq!(result.changed_symbols[0].name, "fn_a");
        assert_eq!(result.affected_symbols.len(), 1);
        assert_eq!(result.affected_symbols[0].name, "fn_b");
        // fn_a has pagerank_score=0.5 (high centrality), which boosts
        // the risk from Low to Medium even with only 1 affected symbol.
        assert_eq!(result.risk_level, RiskLevel::Medium);

        // Verify impact_score is populated: sym_b calls sym_a with confidence 0.9,
        // so impact_score should be 1.0 * 0.9 = 0.9.
        let score = result.affected_symbols[0].impact_score;
        assert!(
            (score - 0.9).abs() < 1e-6,
            "expected impact_score ~0.9, got {score}"
        );
    }

    #[test]
    fn impact_score_decays_through_chain() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        // Build chain: C --0.8--> B --0.9--> A
        // Changing A should affect B (score 0.9) and C (score 0.9 * 0.8 = 0.72).
        let make_sym = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };

        for (uid, name, file) in [
            ("sym:a", "fn_a", "src/a.rs"),
            ("sym:b", "fn_b", "src/b.rs"),
            ("sym:c", "fn_c", "src/c.rs"),
        ] {
            store.insert_symbol(&make_sym(uid, name, file)).unwrap();
        }

        // B calls A (confidence 0.9)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:b".to_string(),
                target_uid: "sym:a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // C calls B (confidence 0.8)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:c".to_string(),
                target_uid: "sym:b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.8,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], 5, None).unwrap();

        assert_eq!(result.affected_symbols.len(), 2);
        // Results should be sorted by impact_score descending.
        assert_eq!(result.affected_symbols[0].name, "fn_b");
        assert!((result.affected_symbols[0].impact_score - 0.9).abs() < 1e-6);
        assert_eq!(result.affected_symbols[1].name, "fn_c");
        assert!((result.affected_symbols[1].impact_score - 0.72).abs() < 1e-6);
    }

    #[test]
    fn low_confidence_chain_pruned_below_threshold() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        // Build chain: C --0.2--> B --0.3--> A
        // B's score = 0.3, C's candidate score = 0.3 * 0.2 = 0.06 < 0.10 threshold.
        // So C should be pruned.
        let make_sym = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        };

        for (uid, name, file) in [
            ("sym:a", "fn_a", "src/a.rs"),
            ("sym:b", "fn_b", "src/b.rs"),
            ("sym:c", "fn_c", "src/c.rs"),
        ] {
            store.insert_symbol(&make_sym(uid, name, file)).unwrap();
        }

        // B calls A (confidence 0.3)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:b".to_string(),
                target_uid: "sym:a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.3,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // C calls B (confidence 0.2)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:c".to_string(),
                target_uid: "sym:b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.2,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], 5, None).unwrap();

        // B is included (score 0.3 >= 0.10), but C is pruned (score 0.06 < 0.10).
        assert_eq!(result.affected_symbols.len(), 1);
        assert_eq!(result.affected_symbols[0].name, "fn_b");
        assert!((result.affected_symbols[0].impact_score - 0.3).abs() < 1e-6);
    }
}
