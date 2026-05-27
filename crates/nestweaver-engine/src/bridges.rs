//! Bridge node detection: finds architectural chokepoints in the code graph.
//!
//! A bridge node sits on many shortest paths between other nodes, giving it
//! high betweenness centrality. These nodes are the critical connectors
//! between different parts of the codebase — changing them has outsized
//! blast radius.
//!
//! Exact betweenness centrality is O(V*E) via Brandes' algorithm. For large
//! graphs we sample a fixed number of source nodes to keep runtime bounded.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result};
use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};

/// Maximum number of source nodes to sample for betweenness centrality.
/// This bounds the algorithm to O(SAMPLE_LIMIT * E) for large graphs.
const SAMPLE_LIMIT: usize = 500;

/// A node identified as a bridge (architectural chokepoint) in the code graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeNode {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub betweenness_score: f64,
    pub communities_connected: Vec<u32>,
}

/// Loaded graph data reusable across bridge detection and community attachment.
pub(crate) struct LoadedGraph {
    pub symbols: Vec<nestweaver_store::SymbolBasic>,
    pub adj: Vec<Vec<usize>>,
    pub uid_to_idx: HashMap<String, usize>,
}

/// Load the code graph and build undirected adjacency data.
fn load_graph(store: &GraphStore) -> Result<Option<LoadedGraph>> {
    let (symbols, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!(e))
        .context("failed to load graph data for bridge detection")?;

    if symbols.is_empty() {
        return Ok(None);
    }

    let uid_to_idx: HashMap<String, usize> = symbols
        .iter()
        .enumerate()
        .map(|(i, s)| (s.uid.clone(), i))
        .collect();

    let n = symbols.len();
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for (src, dst, _confidence) in &edges {
        if let (Some(&si), Some(&di)) = (uid_to_idx.get(src.as_str()), uid_to_idx.get(dst.as_str()))
        {
            adj[si].push(di);
            adj[di].push(si);
        }
    }

    for neighbors in &mut adj {
        neighbors.sort_unstable();
        neighbors.dedup();
    }

    Ok(Some(LoadedGraph {
        symbols,
        adj,
        uid_to_idx,
    }))
}

/// Find the top-N bridge nodes in the code graph, ranked by betweenness centrality.
///
/// Uses Brandes' algorithm with sampling: for each of up to `SAMPLE_LIMIT`
/// randomly-selected source nodes, runs BFS to compute shortest-path counts
/// and accumulates betweenness contributions. The result is normalized by
/// the number of sources sampled.
pub fn find_bridge_nodes(store: &GraphStore, top_n: usize) -> Result<Vec<BridgeNode>> {
    let graph = match load_graph(store)? {
        Some(g) => g,
        None => return Ok(vec![]),
    };

    let n = graph.symbols.len();

    // Compute betweenness centrality via Brandes' algorithm with sampling.
    let betweenness = brandes_sampled(&graph.adj, n);

    // Build bridge nodes.
    let mut bridges: Vec<BridgeNode> = graph
        .symbols
        .iter()
        .enumerate()
        .map(|(i, sym)| BridgeNode {
            uid: sym.uid.clone(),
            name: sym.name.clone(),
            file_path: sym.file_path.clone(),
            betweenness_score: betweenness[i],
            communities_connected: vec![],
        })
        .collect();

    // Sort by betweenness descending.
    bridges.sort_by(|a, b| {
        b.betweenness_score
            .partial_cmp(&a.betweenness_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    bridges.truncate(top_n);
    Ok(bridges)
}

/// Attach community connection information to bridge nodes.
///
/// For each bridge node, determines which cluster IDs are reachable through
/// its immediate neighbors, marking it as connecting those communities.
/// Loads the graph once; if you already have a `LoadedGraph`, prefer
/// the internal helper to avoid the extra load.
pub fn attach_communities(
    bridges: &mut [BridgeNode],
    clustering: &crate::cluster_dispatch::ClusteringOutput,
    store: &GraphStore,
) {
    let graph = match load_graph(store) {
        Ok(Some(g)) => g,
        _ => return,
    };

    // Build uid -> cluster_id map.
    let mut uid_to_cluster: HashMap<String, u32> = HashMap::new();
    for community in &clustering.communities {
        for member in &community.members {
            uid_to_cluster.insert(member.uid.clone(), community.id);
        }
    }

    for bridge in bridges.iter_mut() {
        if let Some(&idx) = graph.uid_to_idx.get(bridge.uid.as_str()) {
            let mut connected_clusters: HashSet<u32> = HashSet::new();
            // Include the bridge's own cluster.
            if let Some(&cid) = uid_to_cluster.get(&bridge.uid) {
                connected_clusters.insert(cid);
            }
            // Add clusters of all neighbors.
            for &neighbor_idx in &graph.adj[idx] {
                if let Some(&cid) = uid_to_cluster.get(&graph.symbols[neighbor_idx].uid) {
                    connected_clusters.insert(cid);
                }
            }
            let mut clusters: Vec<u32> = connected_clusters.into_iter().collect();
            clusters.sort_unstable();
            bridge.communities_connected = clusters;
        }
    }
}

/// Brandes' algorithm for betweenness centrality with source sampling.
///
/// For each source node s (up to SAMPLE_LIMIT), runs BFS to compute:
/// - sigma[t]: number of shortest paths from s to t
/// - delta[v]: dependency of s on v
///
/// The betweenness of each node v is the sum of delta[v] across all sources,
/// normalized by the number of sources sampled.
fn brandes_sampled(adj: &[Vec<usize>], n: usize) -> Vec<f64> {
    let mut betweenness = vec![0.0f64; n];

    // Select source nodes: if graph is small enough, use all; otherwise sample.
    let sources: Vec<usize> = if n <= SAMPLE_LIMIT {
        (0..n).collect()
    } else {
        // Deterministic sampling: pick evenly spaced nodes for reproducibility.
        let step = n as f64 / SAMPLE_LIMIT as f64;
        (0..SAMPLE_LIMIT)
            .map(|i| (i as f64 * step) as usize)
            .collect()
    };

    let num_sources = sources.len();

    for &s in &sources {
        // BFS from source s.
        let mut stack: Vec<usize> = Vec::new();
        let mut predecessors: Vec<Vec<usize>> = vec![vec![]; n];
        let mut sigma = vec![0.0f64; n]; // number of shortest paths
        sigma[s] = 1.0;
        let mut dist: Vec<i64> = vec![-1; n];
        dist[s] = 0;

        let mut queue = VecDeque::new();
        queue.push_back(s);

        while let Some(v) = queue.pop_front() {
            stack.push(v);
            for &w in &adj[v] {
                // w found for the first time?
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    queue.push_back(w);
                }
                // Is edge (v, w) on a shortest path to w?
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    predecessors[w].push(v);
                }
            }
        }

        // Accumulation: back-propagate dependencies.
        let mut delta = vec![0.0f64; n];
        while let Some(w) = stack.pop() {
            for &v in &predecessors[w] {
                delta[v] += (sigma[v] / sigma[w]) * (1.0 + delta[w]);
            }
            if w != s {
                betweenness[w] += delta[w];
            }
        }
    }

    // Normalize by the number of sources sampled. For an undirected graph,
    // each pair is counted twice in the BFS, so we also divide by 2.
    if num_sources > 0 {
        let scale = if n <= SAMPLE_LIMIT {
            // Exact computation: standard normalization for undirected graphs.
            0.5
        } else {
            // Sampled: scale up to estimate full betweenness.
            (n as f64) / (2.0 * num_sources as f64)
        };
        for b in &mut betweenness {
            *b *= scale;
        }
    }

    betweenness
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
    fn bridge_in_barbell_graph_ranks_highest() {
        // Barbell graph: two cliques (A-B-C) and (D-E-F) connected by a
        // single bridge node (X). X should have the highest betweenness.
        let store = GraphStore::in_memory().unwrap();

        for uid in ["A", "B", "C", "X", "D", "E", "F"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}"), "src/lib.rs"))
                .unwrap();
        }

        // Clique 1: A-B, B-C, A-C
        store.insert_edge(&make_edge("A", "B")).unwrap();
        store.insert_edge(&make_edge("B", "C")).unwrap();
        store.insert_edge(&make_edge("A", "C")).unwrap();

        // Bridge: C-X, X-D
        store.insert_edge(&make_edge("C", "X")).unwrap();
        store.insert_edge(&make_edge("X", "D")).unwrap();

        // Clique 2: D-E, E-F, D-F
        store.insert_edge(&make_edge("D", "E")).unwrap();
        store.insert_edge(&make_edge("E", "F")).unwrap();
        store.insert_edge(&make_edge("D", "F")).unwrap();

        let bridges = find_bridge_nodes(&store, 3).unwrap();
        assert!(!bridges.is_empty());
        // X should be the top bridge since all paths between the two
        // cliques pass through it.
        assert_eq!(
            bridges[0].uid, "X",
            "bridge node X should rank first, got {}",
            bridges[0].uid
        );
        assert!(
            bridges[0].betweenness_score > 0.0,
            "bridge should have positive betweenness"
        );
    }

    #[test]
    fn empty_graph_returns_empty() {
        let store = GraphStore::in_memory().unwrap();
        let bridges = find_bridge_nodes(&store, 10).unwrap();
        assert!(bridges.is_empty());
    }

    #[test]
    fn linear_chain_middle_node_is_bridge() {
        // Linear chain: A -> B -> C -> D -> E
        // B, C, D are all bridges; C should have highest betweenness.
        let store = GraphStore::in_memory().unwrap();

        for uid in ["A", "B", "C", "D", "E"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}"), "src/lib.rs"))
                .unwrap();
        }
        store.insert_edge(&make_edge("A", "B")).unwrap();
        store.insert_edge(&make_edge("B", "C")).unwrap();
        store.insert_edge(&make_edge("C", "D")).unwrap();
        store.insert_edge(&make_edge("D", "E")).unwrap();

        let bridges = find_bridge_nodes(&store, 5).unwrap();
        // C is in the middle of the chain — it should be on the most
        // shortest paths.
        let c = bridges.iter().find(|b| b.uid == "C").unwrap();
        let a = bridges.iter().find(|b| b.uid == "A").unwrap();
        assert!(
            c.betweenness_score > a.betweenness_score,
            "middle node C ({:.2}) should have higher betweenness than endpoint A ({:.2})",
            c.betweenness_score,
            a.betweenness_score
        );
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
        for i in 0..9 {
            store
                .insert_edge(&make_edge(&format!("s{i}"), &format!("s{}", i + 1)))
                .unwrap();
        }

        let bridges = find_bridge_nodes(&store, 3).unwrap();
        assert_eq!(bridges.len(), 3);
    }
}
