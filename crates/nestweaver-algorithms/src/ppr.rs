use std::collections::{HashMap, HashSet, VecDeque};

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
            // Anti-feedback-loop / exploration floor: we scale every seed's
            // personalization weight by `(1 - w)` and only redistribute the
            // remaining `w` (default 5%) according to interaction history.
            // Because `w < 1`, a seed with NO interaction history still
            // retains `(1 - w)` of its original personalization mass — it can
            // never be driven to zero by the interaction blend. This keeps
            // newly-seeded, never-before-accessed nodes discoverable and
            // prevents the feedback loop from collapsing onto historically
            // popular nodes.
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

/// Run Personalized PageRank using the Forward Push (LocalPush) algorithm.
///
/// Produces the same output shape as [`personalized_pagerank`] — `(uid, score)`
/// pairs sorted descending — but avoids iterating over every node each step.
/// Instead it maintains a residual vector and only pushes mass from nodes whose
/// residual exceeds a threshold, making it significantly faster on sparse
/// graphs where PPR mass concentrates near the seeds.
///
/// Seeds are always included in the output regardless of score; non-seed nodes
/// must exceed `config.min_score`.
pub fn forward_push_ppr(
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

    // -- Build outgoing edge list from incoming edges --
    // adjacency.incoming[v] contains (u, w) meaning u→v with weight w.
    // We need outgoing[u] = [(v, w)] for forward pushes.
    let mut outgoing: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for v in 0..n {
        for &(u, w) in &adjacency.incoming[v] {
            outgoing[u].push((v, w));
        }
    }

    // -- Build personalization vector (identical to power iteration) --
    let personalization_val = 1.0 / seed_count as f64;
    let mut personalization: Vec<f64> = (0..n)
        .map(|i| {
            if seed_set.contains(&i) {
                personalization_val
            } else {
                0.0
            }
        })
        .collect();

    // -- Apply interaction memory bias (same exploration floor pattern) --
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

    // -- Forward Push --
    let alpha = 1.0 - config.damping; // teleport probability
    let r_max = 1e-6; // residual threshold (matches power-iteration convergence)

    let mut estimate = vec![0.0f64; n];
    let mut residual = personalization.clone();

    // Seed the queue with nodes that have nonzero residual.
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut in_queue = vec![false; n];
    for i in 0..n {
        if residual[i] > 0.0 {
            queue.push_back(i);
            in_queue[i] = true;
        }
    }

    // Safety limit to prevent runaway on adversarial graphs.
    let max_pushes = n * 10;
    let mut push_count = 0;

    while let Some(v) = queue.pop_front() {
        in_queue[v] = false;
        push_count += 1;
        if push_count > max_pushes {
            break;
        }

        let r_v = residual[v];
        if r_v.abs() < r_max {
            continue;
        }

        // Absorb: estimate accumulates α * residual (teleport fraction).
        estimate[v] += alpha * r_v;

        // Push: distribute (1 - α) * residual to outgoing neighbours.
        if adjacency.out_weight[v] > 0.0 {
            let push_mass = (1.0 - alpha) * r_v;
            for &(u, w) in &outgoing[v] {
                let delta = push_mass * w / adjacency.out_weight[v];
                residual[u] += delta;
                if !in_queue[u] && residual[u].abs() > r_max {
                    queue.push_back(u);
                    in_queue[u] = true;
                }
            }
        } else {
            // Dangling node: redistribute through the personalization vector
            // (same semantics as power iteration's dangling-sum handling).
            let push_mass = (1.0 - alpha) * r_v;
            for (i, &p) in personalization.iter().enumerate() {
                if p > 0.0 {
                    residual[i] += push_mass * p;
                    if !in_queue[i] && residual[i].abs() > r_max {
                        queue.push_back(i);
                        in_queue[i] = true;
                    }
                }
            }
        }

        residual[v] = 0.0;
    }

    // -- Collect results (same filtering as power iteration) --
    let mut results: Vec<(String, f64)> = uids
        .iter()
        .enumerate()
        .filter(|&(i, _)| seed_set.contains(&i) || estimate[i] > config.min_score)
        .map(|(i, uid)| (uid.clone(), estimate[i]))
        .collect();

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
    fn exploration_floor_keeps_non_interacted_seed_nonzero() {
        // Seed from "a" (which has NO interaction history) while a heavy
        // interaction score is loaded for an unrelated node "c". The blend
        // must NOT drive a's personalization weight to zero — a must still
        // retain its (1 - w) share so it stays discoverable.
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
            // No edges: dangling seed so its score reflects its
            // personalization weight directly.
            edges: vec![],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());

        let mut interaction_scores = HashMap::new();
        interaction_scores.insert("c".to_string(), 1_000_000.0);
        let w = 0.05;
        let config = PprConfig {
            interaction_scores: Some(interaction_scores),
            interaction_bias_weight: w,
            ..PprConfig::default()
        };

        let result = personalized_pagerank(&graph.uids, &adj, &["a".to_string()], &config);
        let a_score = result.iter().find(|(uid, _)| uid == "a").map(|(_, s)| *s);

        // "a" must appear (seeds are always included) and must be strictly
        // positive — at least its (1 - w) seed share survives the blend.
        let a_score = a_score.expect("seed 'a' should always be present");
        assert!(
            a_score >= 1.0 - w - 1e-6,
            "non-interacted seed should keep >= (1-w) of its mass, got {a_score}"
        );
        assert!(
            a_score > 0.0,
            "exploration floor violated: a_score={a_score}"
        );
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

    // ---- Forward Push PPR tests ----

    #[test]
    fn forward_push_empty_graph() {
        let graph = InMemoryGraph {
            uids: vec![],
            nodes: vec![],
            edges: vec![],
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let result = forward_push_ppr(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());
        assert!(result.is_empty());
    }

    #[test]
    fn forward_push_no_matching_seeds() {
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
        let result = forward_push_ppr(
            &graph.uids,
            &adj,
            &["nonexistent".to_string()],
            &PprConfig::default(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn forward_push_seeds_rank_highest() {
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
        let result = forward_push_ppr(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());

        // Seed "a" should appear and have a positive score.
        let a_score = result.iter().find(|(uid, _)| uid == "a").unwrap().1;
        assert!(a_score > 0.0);

        // All nodes should appear.
        assert!(result.iter().any(|(uid, _)| uid == "a"));
        assert!(result.iter().any(|(uid, _)| uid == "b"));
        assert!(result.iter().any(|(uid, _)| uid == "c"));

        // b (closer to seed) should score higher than c.
        let b_score = result.iter().find(|(uid, _)| uid == "b").unwrap().1;
        let c_score = result.iter().find(|(uid, _)| uid == "c").unwrap().1;
        assert!(b_score > c_score);
    }

    #[test]
    fn forward_push_matches_power_iteration_top_nodes() {
        // Build a graph with 15 nodes in a mix of chain and fan-out patterns.
        // Compare top results between both algorithms: expect significant overlap.
        let n = 15;
        let uids: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
        let nodes: Vec<NodeMeta> = (0..n)
            .map(|i| NodeMeta {
                name: format!("n{i}"),
                kind: "Function".into(),
                file_path: None,
                pagerank_score: None,
                is_entry_point: false,
            })
            .collect();

        // Chain: 0->1->2->3->4->5
        // Fan-out from 0: 0->6, 0->7, 0->8
        // Cross links: 3->9, 5->10, 7->11, 8->12
        // Extra: 9->13, 13->14
        let edges = vec![
            (0, 1, 1.0, EdgeKind::Calls),
            (1, 2, 1.0, EdgeKind::Calls),
            (2, 3, 1.0, EdgeKind::Calls),
            (3, 4, 1.0, EdgeKind::Calls),
            (4, 5, 1.0, EdgeKind::Calls),
            (0, 6, 1.0, EdgeKind::Calls),
            (0, 7, 1.0, EdgeKind::Calls),
            (0, 8, 1.0, EdgeKind::Calls),
            (3, 9, 1.0, EdgeKind::Accesses),
            (5, 10, 1.0, EdgeKind::Accesses),
            (7, 11, 1.0, EdgeKind::Calls),
            (8, 12, 1.0, EdgeKind::Calls),
            (9, 13, 1.0, EdgeKind::Calls),
            (13, 14, 1.0, EdgeKind::Calls),
        ];

        let graph = InMemoryGraph {
            uids: uids.clone(),
            nodes,
            edges,
            generation: 0,
        };
        let adj = graph.build_adjacency(&EdgeWeightConfig::default_config());
        let seeds = vec!["n0".to_string()];
        let config = PprConfig::default();

        let pi_result = personalized_pagerank(&uids, &adj, &seeds, &config);
        let fp_result = forward_push_ppr(&uids, &adj, &seeds, &config);

        // Both should be non-empty.
        assert!(!pi_result.is_empty());
        assert!(!fp_result.is_empty());

        // Compare top-10 overlap (or fewer if results are shorter).
        let take = 10.min(pi_result.len()).min(fp_result.len());
        let pi_top: HashSet<&str> = pi_result[..take]
            .iter()
            .map(|(uid, _)| uid.as_str())
            .collect();
        let fp_top: HashSet<&str> = fp_result[..take]
            .iter()
            .map(|(uid, _)| uid.as_str())
            .collect();
        let overlap = pi_top.intersection(&fp_top).count();

        assert!(
            overlap >= take * 7 / 10,
            "Expected >= 70% overlap in top-{take}, got {overlap}/{take}. \
             PI top: {pi_top:?}, FP top: {fp_top:?}"
        );
    }

    #[test]
    fn forward_push_single_dangling_node() {
        // Single node with no edges: all mass stays on the seed.
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
        let result = forward_push_ppr(&graph.uids, &adj, &["a".to_string()], &PprConfig::default());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "a");
        assert!(result[0].1 > 0.0);
    }
}
