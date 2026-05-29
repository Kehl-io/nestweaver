use std::collections::{HashMap, VecDeque};

use crate::graph::InMemoryGraph;

pub struct ImpactNode {
    pub uid: String,
    pub depth: u32,
}

/// BFS from seed nodes through the graph's forward edges.
/// Returns all reachable nodes with their depth (hop count from nearest seed).
pub fn impact_analysis(
    graph: &InMemoryGraph,
    seed_uids: &[String],
    max_depth: u32,
    confidence_threshold: f32,
) -> Vec<ImpactNode> {
    let n = graph.uids.len();
    let uid_to_idx: HashMap<&str, usize> = graph
        .uids
        .iter()
        .enumerate()
        .map(|(i, uid)| (uid.as_str(), i))
        .collect();

    // Build forward adjacency list (only edges above confidence threshold)
    let mut forward: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(src, tgt, conf, _kind) in &graph.edges {
        if conf >= confidence_threshold {
            let si = src as usize;
            let ti = tgt as usize;
            if si < n && ti < n {
                forward[si].push(ti);
            }
        }
    }

    // BFS from seeds
    let mut visited: HashMap<usize, u32> = HashMap::new();
    let mut queue: VecDeque<(usize, u32)> = VecDeque::new();

    for seed in seed_uids {
        if let Some(&idx) = uid_to_idx.get(seed.as_str())
            && let std::collections::hash_map::Entry::Vacant(e) = visited.entry(idx)
        {
            e.insert(0);
            queue.push_back((idx, 0));
        }
    }

    while let Some((node, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for &neighbor in &forward[node] {
            if let std::collections::hash_map::Entry::Vacant(e) = visited.entry(neighbor) {
                e.insert(depth + 1);
                queue.push_back((neighbor, depth + 1));
            }
        }
    }

    let mut results: Vec<ImpactNode> = visited
        .into_iter()
        .map(|(idx, depth)| ImpactNode {
            uid: graph.uids[idx].clone(),
            depth,
        })
        .collect();

    results.sort_by_key(|node| node.depth);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeKind, NodeMeta};

    fn make_node(name: &str) -> NodeMeta {
        NodeMeta {
            name: name.to_string(),
            kind: "function".to_string(),
            file_path: None,
            pagerank_score: None,
            is_entry_point: false,
        }
    }

    fn make_graph(
        uids: Vec<&str>,
        edges: Vec<(u32, u32, f32, EdgeKind)>,
    ) -> InMemoryGraph {
        InMemoryGraph {
            uids: uids.iter().map(|s| s.to_string()).collect(),
            nodes: uids.iter().map(|s| make_node(s)).collect(),
            edges,
            generation: 0,
        }
    }

    #[test]
    fn empty_graph_returns_empty() {
        let graph = make_graph(vec![], vec![]);
        let results = impact_analysis(&graph, &["a".to_string()], 5, 0.0);
        assert!(results.is_empty());
    }

    #[test]
    fn single_seed_returns_seed_at_depth_zero() {
        let graph = make_graph(vec!["a", "b"], vec![]);
        let results = impact_analysis(&graph, &["a".to_string()], 5, 0.0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].uid, "a");
        assert_eq!(results[0].depth, 0);
    }

    #[test]
    fn chain_max_depth_one_returns_two_nodes() {
        // A(0) -> B(1) -> C(2)
        let graph = make_graph(
            vec!["A", "B", "C"],
            vec![
                (0, 1, 1.0, EdgeKind::Calls),
                (1, 2, 1.0, EdgeKind::Calls),
            ],
        );
        let results = impact_analysis(&graph, &["A".to_string()], 1, 0.0);
        assert_eq!(results.len(), 2);
        let depths: std::collections::HashMap<&str, u32> =
            results.iter().map(|n| (n.uid.as_str(), n.depth)).collect();
        assert_eq!(depths["A"], 0);
        assert_eq!(depths["B"], 1);
        assert!(!depths.contains_key("C"));
    }

    #[test]
    fn chain_max_depth_two_returns_all_three() {
        // A(0) -> B(1) -> C(2)
        let graph = make_graph(
            vec!["A", "B", "C"],
            vec![
                (0, 1, 1.0, EdgeKind::Calls),
                (1, 2, 1.0, EdgeKind::Calls),
            ],
        );
        let results = impact_analysis(&graph, &["A".to_string()], 2, 0.0);
        assert_eq!(results.len(), 3);
        let depths: std::collections::HashMap<&str, u32> =
            results.iter().map(|n| (n.uid.as_str(), n.depth)).collect();
        assert_eq!(depths["A"], 0);
        assert_eq!(depths["B"], 1);
        assert_eq!(depths["C"], 2);
    }

    #[test]
    fn confidence_threshold_filters_low_confidence_edges() {
        // A -> B at 0.3 confidence, A -> C at 0.9 confidence
        let graph = make_graph(
            vec!["A", "B", "C"],
            vec![
                (0, 1, 0.3, EdgeKind::Calls),
                (0, 2, 0.9, EdgeKind::Calls),
            ],
        );
        // Threshold 0.5 should only traverse edge to C
        let results = impact_analysis(&graph, &["A".to_string()], 5, 0.5);
        let uids: Vec<&str> = results.iter().map(|n| n.uid.as_str()).collect();
        assert!(uids.contains(&"A"));
        assert!(uids.contains(&"C"));
        assert!(!uids.contains(&"B"));
    }
}
