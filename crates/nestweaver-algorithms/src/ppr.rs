use std::collections::{HashMap, HashSet};

use crate::graph::AdjacencyData;

/// Configuration for a PPR run.
pub struct PprConfig {
    /// Damping factor (probability of following an edge vs teleporting).
    pub damping: f64,
    /// Maximum number of power-iteration steps.
    pub max_iterations: u32,
    /// Minimum score threshold — non-seed nodes below this are excluded from
    /// results.
    pub min_score: f64,
    /// Optional interaction memory scores for personalization bias. When
    /// present, a small fraction of the personalization vector is blended
    /// from these scores so that frequently-accessed nodes receive a slight
    /// ranking boost.
    pub interaction_scores: Option<HashMap<String, f64>>,
    /// Weight of the interaction bias blend (default 0.05 = 5%).
    pub interaction_bias_weight: f64,
}

impl Default for PprConfig {
    fn default() -> Self {
        Self {
            damping: 0.75,
            max_iterations: 20,
            min_score: 1e-4,
            interaction_scores: None,
            interaction_bias_weight: 0.05,
        }
    }
}

/// Run Personalized PageRank on pre-built adjacency data.
///
/// Returns `(uid, score)` pairs sorted descending by score. Seed nodes are
/// always included regardless of score; non-seed nodes must exceed
/// `config.min_score` to appear in the output.
///
/// This is a pure-compute function with no database or I/O dependencies,
/// making it suitable for WASM compilation.
pub fn personalized_pagerank(
    uids: &[String],
    adjacency: &AdjacencyData,
    seed_uids: &[String],
    config: &PprConfig,
) -> Vec<(String, f64)> {
    let n = uids.len();
    if n == 0 {
        return vec![];
    }

    let seed_set: HashSet<usize> = seed_uids
        .iter()
        .filter_map(|uid| adjacency.uid_to_idx.get(uid).copied())
        .collect();

    let seed_count = seed_set.len();
    if seed_count == 0 {
        return vec![];
    }

    let personalization_val = 1.0 / seed_count as f64;

    // Build personalization vector: 1/|seeds| for seeds, 0 otherwise.
    let mut personalization: Vec<f64> = (0..n)
        .map(|i| {
            if seed_set.contains(&i) {
                personalization_val
            } else {
                0.0
            }
        })
        .collect();

    // Apply interaction memory bias (conservative: 5% weight by default).
    // Blends a small fraction of interaction history scores into the
    // personalization vector so frequently-accessed nodes receive a slight
    // ranking boost without overwhelming seed-based relevance.
    if let Some(ref scores) = config.interaction_scores {
        let mut interaction_mass = 0.0;
        let mut contributions: Vec<(usize, f64)> = Vec::new();
        for (i, uid) in uids.iter().enumerate() {
            if let Some(&score) = scores.get(uid)
                && score > 0.0
            {
                contributions.push((i, score));
                interaction_mass += score;
            }
        }
        if interaction_mass > 0.0 {
            for p in personalization.iter_mut() {
                *p *= 1.0 - config.interaction_bias_weight;
            }
            for (i, score) in &contributions {
                personalization[*i] += config.interaction_bias_weight * score / interaction_mass;
            }
        }
    }

    // PPR power iteration.
    let mut scores = personalization.clone();
    for _ in 0..config.max_iterations {
        let mut new_scores = vec![0.0f64; n];

        // Dangling-node handling: redistribute mass from nodes with no
        // outgoing edges through the personalization vector.
        let dangling_sum: f64 = scores
            .iter()
            .enumerate()
            .filter(|&(i, _)| adjacency.out_weight[i] == 0.0)
            .map(|(_, &s)| s)
            .sum();

        for v in 0..n {
            new_scores[v] = (1.0 - config.damping) * personalization[v]
                + config.damping * dangling_sum * personalization[v];

            for &(u, w) in &adjacency.incoming[v] {
                if adjacency.out_weight[u] > 0.0 {
                    new_scores[v] += config.damping * scores[u] * w / adjacency.out_weight[u];
                }
            }
        }

        // Check convergence (max absolute change).
        let delta: f64 = new_scores
            .iter()
            .zip(scores.iter())
            .map(|(n, o)| (n - o).abs())
            .fold(0.0_f64, f64::max);

        scores = new_scores;
        if delta < 1e-6 {
            break;
        }
    }

    // Collect results: always include seeds, filter others by min_score.
    let mut results: Vec<(String, f64)> = uids
        .iter()
        .enumerate()
        .filter(|&(i, _)| seed_set.contains(&i) || scores[i] > config.min_score)
        .map(|(i, uid)| (uid.clone(), scores[i]))
        .collect();

    // Sort descending by score.
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::*;

    #[test]
    fn empty_graph_returns_empty() {
        let graph = InMemoryGraph {
            uids: vec![],
            nodes: vec![],
            edges: vec![],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let result =
            personalized_pagerank(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());
        assert!(result.is_empty());
    }

    #[test]
    fn single_node_returns_seed() {
        let graph = InMemoryGraph {
            uids: vec!["a".to_string()],
            nodes: vec![NodeMeta {
                name: "a".into(),
                kind: "Function".into(),
                file_path: None,
                pagerank_score: None,
                is_entry_point: false,
            }],
            edges: vec![],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let result =
            personalized_pagerank(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "a");
        // Single dangling node with personalization 1.0: all mass stays on seed.
        assert!((result[0].1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn no_seeds_in_graph_returns_empty() {
        let graph = InMemoryGraph {
            uids: vec!["a".to_string()],
            nodes: vec![NodeMeta {
                name: "a".into(),
                kind: "Function".into(),
                file_path: None,
                pagerank_score: None,
                is_entry_point: false,
            }],
            edges: vec![],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let result = personalized_pagerank(
            &graph.uids,
            &adj,
            &["nonexistent".to_string()],
            &PprConfig::default(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn chain_graph_includes_all_nodes() {
        let graph = InMemoryGraph {
            uids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            nodes: vec![
                NodeMeta {
                    name: "a".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "b".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "c".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
            ],
            edges: vec![(0, 1, 1.0, EdgeKind::Calls), (1, 2, 1.0, EdgeKind::Calls)],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let result =
            personalized_pagerank(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());
        // All nodes in the chain should appear (seed always included,
        // neighbours receive propagated mass).
        assert!(result.iter().any(|(uid, _)| uid == "a"));
        assert!(result.iter().any(|(uid, _)| uid == "b"));
        assert!(result.iter().any(|(uid, _)| uid == "c"));
        // Seed node score should be positive.
        let a_score = result.iter().find(|(uid, _)| uid == "a").unwrap().1;
        assert!(a_score > 0.0);
    }

    #[test]
    fn chain_graph_propagates_to_neighbours() {
        let graph = InMemoryGraph {
            uids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            nodes: vec![
                NodeMeta {
                    name: "a".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "b".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "c".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
            ],
            edges: vec![(0, 1, 1.0, EdgeKind::Calls), (1, 2, 1.0, EdgeKind::Calls)],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let result =
            personalized_pagerank(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());
        // b should be in results (direct neighbour of seed)
        assert!(result.iter().any(|(uid, _)| uid == "b"));
        // b should score higher than c (closer to seed)
        let b_score = result.iter().find(|(uid, _)| uid == "b").unwrap().1;
        let c_score = result.iter().find(|(uid, _)| uid == "c").unwrap().1;
        assert!(b_score > c_score);
    }

    #[test]
    fn edge_kind_weights_affect_adjacency() {
        // Verify that the adjacency data reflects different base weights
        // for different edge kinds. Calls (1.0) should produce higher
        // outgoing weight than Accesses (0.4).
        let make_graph = |kind: EdgeKind| -> InMemoryGraph {
            InMemoryGraph {
                uids: vec!["a".to_string(), "b".to_string()],
                nodes: vec![
                    NodeMeta {
                        name: "a".into(),
                        kind: "Function".into(),
                        file_path: None,
                        pagerank_score: None,
                        is_entry_point: false,
                    },
                    NodeMeta {
                        name: "b".into(),
                        kind: "Function".into(),
                        file_path: None,
                        pagerank_score: None,
                        is_entry_point: false,
                    },
                ],
                edges: vec![(0, 1, 1.0, kind)],
                generation: 0,
            }
        };

        let weights = EdgeWeightConfig::default_config();

        let calls_graph = make_graph(EdgeKind::Calls);
        let calls_adj = calls_graph.build_adjacency(&weights);

        let access_graph = make_graph(EdgeKind::Accesses);
        let access_adj = access_graph.build_adjacency(&weights);

        // Calls base weight is 1.0, Accesses is 0.4 — forward outgoing
        // weight for node "a" (idx 0) should reflect this difference.
        assert!(calls_adj.out_weight[0] > access_adj.out_weight[0]);
        // Specifically: calls forward weight = 1.0, reverse on a = 0.3
        // so out_weight[0] = 1.0 + 0.3 = ... wait, only forward from 0.
        // out_weight[0] for calls = 1.0 (forward a->b) + 0.3 (reverse b->a adds to b's out, not a)
        // Actually: forward (0,1,1.0) => out_weight[0] += 1.0; reverse (1,0,0.3) => out_weight[1] += 0.3
        // So out_weight[0] for Calls = 1.0, for Accesses = 0.4
        let ratio = calls_adj.out_weight[0] / access_adj.out_weight[0];
        assert!((ratio - 2.5).abs() < 0.01); // 1.0 / 0.4 = 2.5
    }

    #[test]
    fn interaction_scores_bias_personalization() {
        let graph = InMemoryGraph {
            uids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            nodes: vec![
                NodeMeta {
                    name: "a".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "b".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "c".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
            ],
            edges: vec![(0, 1, 1.0, EdgeKind::Calls), (0, 2, 1.0, EdgeKind::Calls)],
            generation: 0,
        };

        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());

        // Without interaction scores
        let result_without =
            personalized_pagerank(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());

        // With interaction scores boosting "c"
        let mut interaction_scores = HashMap::new();
        interaction_scores.insert("c".to_string(), 10.0);
        let config_with = PprConfig {
            interaction_scores: Some(interaction_scores),
            ..PprConfig::default()
        };
        let result_with =
            personalized_pagerank(&graph.uids, &adj, &["a".to_string()], &config_with);

        // c should score higher with interaction bias than without
        let c_without = result_without.iter().find(|(uid, _)| uid == "c").unwrap().1;
        let c_with = result_with.iter().find(|(uid, _)| uid == "c").unwrap().1;
        assert!(c_with > c_without);
    }

    #[test]
    fn msgpack_roundtrip() {
        let graph = InMemoryGraph {
            uids: vec!["x".to_string()],
            nodes: vec![NodeMeta {
                name: "x".into(),
                kind: "Class".into(),
                file_path: Some("src/x.rs".into()),
                pagerank_score: Some(0.5),
                is_entry_point: true,
            }],
            edges: vec![(0, 0, 1.0, EdgeKind::Calls)],
            generation: 42,
        };
        let bytes = rmp_serde::to_vec(&graph).unwrap();
        let decoded: InMemoryGraph = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.uids, graph.uids);
        assert_eq!(decoded.generation, 42);
        assert_eq!(decoded.nodes[0].name, "x");
        assert!(decoded.nodes[0].is_entry_point);
        assert_eq!(decoded.edges.len(), 1);
        assert_eq!(decoded.edges[0].3, EdgeKind::Calls);
    }

    #[test]
    fn multiple_seeds_distribute_personalization() {
        let graph = InMemoryGraph {
            uids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            nodes: vec![
                NodeMeta {
                    name: "a".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "b".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
                NodeMeta {
                    name: "c".into(),
                    kind: "Function".into(),
                    file_path: None,
                    pagerank_score: None,
                    is_entry_point: false,
                },
            ],
            edges: vec![(0, 2, 1.0, EdgeKind::Calls), (1, 2, 1.0, EdgeKind::Calls)],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let result = personalized_pagerank(
            &graph.uids,
            &adj,
            &["a".to_string(), "b".to_string()],
            &PprConfig::default(),
        );
        // Both seeds should be in results
        assert!(result.iter().any(|(uid, _)| uid == "a"));
        assert!(result.iter().any(|(uid, _)| uid == "b"));
        // c (called by both seeds) should also appear
        assert!(result.iter().any(|(uid, _)| uid == "c"));
    }
}
