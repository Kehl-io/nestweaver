//! Hub node detection: finds the most connected nodes in the code graph.
//!
//! A hub is a symbol with high degree centrality (many incoming + outgoing
//! edges) and/or high PageRank. Hubs represent central abstractions that
//! many parts of the codebase depend on.

use std::collections::HashMap;

use anyhow::{Context, Result};
use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};

/// A node identified as a hub in the code graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubNode {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub in_degree: usize,
    pub out_degree: usize,
    pub total_degree: usize,
    pub pagerank_score: f64,
    pub cluster_id: Option<u32>,
}

/// Find the top-N hub nodes in the code graph, ranked by total degree.
///
/// Loads all symbols and code edges from the store, computes in/out/total
/// degree for each symbol, attaches PageRank scores from the in-memory
/// cache, and optionally attaches cluster IDs from a cached clustering
/// result. Returns the top-N by total degree descending.
///
/// **Performance note**: PageRank scores are read from the in-memory cache
/// (fast). The bottleneck is `load_code_symbols_and_edges` which loads all
/// symbols and edges from the database to compute degree counts. On large
/// graphs (80K+ symbols), this takes ~500-700ms, dominated by DB I/O.
/// Degree counts require the full edge set and cannot use the PageRank
/// cache (which stores centrality, not in/out degree).
pub fn find_hub_nodes(store: &GraphStore, top_n: usize) -> Result<Vec<HubNode>> {
    let (symbols, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!(e))
        .context("failed to load graph data for hub detection")?;

    if symbols.is_empty() {
        return Ok(vec![]);
    }

    // Build UID -> index mapping.
    let uid_to_idx: HashMap<&str, usize> = symbols
        .iter()
        .enumerate()
        .map(|(i, s)| (s.uid.as_str(), i))
        .collect();

    let n = symbols.len();
    let mut in_degree = vec![0usize; n];
    let mut out_degree = vec![0usize; n];

    for (src, dst, _confidence) in &edges {
        if let (Some(&si), Some(&di)) = (uid_to_idx.get(src.as_str()), uid_to_idx.get(dst.as_str()))
        {
            out_degree[si] += 1;
            in_degree[di] += 1;
        }
    }

    // Read PageRank scores from the in-memory cache.
    let pr_scores: HashMap<String, f64> = store.pagerank_scores();

    // Build hub nodes.
    let mut hubs: Vec<HubNode> = symbols
        .iter()
        .enumerate()
        .map(|(i, sym)| {
            let total = in_degree[i] + out_degree[i];
            let pagerank = pr_scores.get(&sym.uid).copied().unwrap_or(0.0);
            HubNode {
                uid: sym.uid.clone(),
                name: sym.name.clone(),
                file_path: sym.file_path.clone(),
                in_degree: in_degree[i],
                out_degree: out_degree[i],
                total_degree: total,
                pagerank_score: pagerank,
                cluster_id: None,
            }
        })
        .collect();

    // Sort by total degree descending, break ties by PageRank descending.
    hubs.sort_by(|a, b| {
        b.total_degree.cmp(&a.total_degree).then_with(|| {
            b.pagerank_score
                .partial_cmp(&a.pagerank_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    hubs.truncate(top_n);
    Ok(hubs)
}

/// Attach cluster IDs to hub nodes from a cached clustering result.
///
/// Mutates the nodes in place. If no clustering output is provided,
/// cluster_id remains None.
pub fn attach_cluster_ids(
    hubs: &mut [HubNode],
    clustering: &crate::cluster_dispatch::ClusteringOutput,
) {
    // Build uid -> cluster_id map from clustering output.
    let mut uid_to_cluster: HashMap<&str, u32> = HashMap::new();
    for community in &clustering.communities {
        for member in &community.members {
            uid_to_cluster.insert(member.uid.as_str(), community.id);
        }
    }
    for hub in hubs.iter_mut() {
        hub.cluster_id = uid_to_cluster.get(hub.uid.as_str()).copied();
    }
}

#[cfg(test)]
mod tests {
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};
    use nestweaver_store::GraphStore;

    use super::*;

    fn make_symbol(uid: &str, name: &str, file_path: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo-1".to_string(),
            file_path: file_path.to_string(),
            start_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        }
    }

    fn make_edge(src: &str, tgt: &str) -> ResolvedEdge {
        ResolvedEdge {
            source_uid: src.to_string(),
            target_uid: tgt.to_string(),
            edge_type: EdgeType::Calls,
            confidence: 1.0,
            link_type: None,
        }
    }

    #[test]
    fn hub_with_most_connections_ranks_first() {
        let store = GraphStore::in_memory().unwrap();

        // Create a star topology: Hub connects to A, B, C, D.
        store
            .insert_symbol(&make_symbol("hub", "hub_fn", "src/hub.rs"))
            .unwrap();
        for name in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(name, &format!("fn_{name}"), "src/leaf.rs"))
                .unwrap();
            store.insert_edge(&make_edge("hub", name)).unwrap();
        }
        // Also A -> B edge so A has some connections too.
        store.insert_edge(&make_edge("A", "B")).unwrap();

        let hubs = find_hub_nodes(&store, 3).unwrap();
        assert!(!hubs.is_empty());
        assert_eq!(hubs[0].uid, "hub", "hub should rank first");
        assert_eq!(hubs[0].out_degree, 4);
    }

    #[test]
    fn empty_graph_returns_empty() {
        let store = GraphStore::in_memory().unwrap();
        let hubs = find_hub_nodes(&store, 10).unwrap();
        assert!(hubs.is_empty());
    }

    #[test]
    fn top_n_limits_output() {
        let store = GraphStore::in_memory().unwrap();
        for i in 0..10 {
            store
                .insert_symbol(&make_symbol(
                    &format!("s{i}"),
                    &format!("fn_{i}"),
                    "src/lib.rs",
                ))
                .unwrap();
        }
        // Chain: s0 -> s1 -> s2 -> ... -> s9
        for i in 0..9 {
            store
                .insert_edge(&make_edge(&format!("s{i}"), &format!("s{}", i + 1)))
                .unwrap();
        }

        let hubs = find_hub_nodes(&store, 3).unwrap();
        assert_eq!(hubs.len(), 3);
    }

    #[test]
    fn hub_detection_counts_both_directions() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("center", "center", "src/center.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("caller", "caller", "src/caller.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("callee", "callee", "src/callee.rs"))
            .unwrap();

        // caller -> center -> callee
        store.insert_edge(&make_edge("caller", "center")).unwrap();
        store.insert_edge(&make_edge("center", "callee")).unwrap();

        let hubs = find_hub_nodes(&store, 10).unwrap();
        let center = hubs.iter().find(|h| h.uid == "center").unwrap();
        assert_eq!(center.in_degree, 1);
        assert_eq!(center.out_degree, 1);
        assert_eq!(center.total_degree, 2);
    }
}
