// Louvain-style local-moving community detection for code call graphs.
// Clusters graph nodes into functional communities to enable process-grouped search.
//
// NOTE: this is single-level local moving (the Louvain first phase), NOT the
// full Leiden algorithm — there is no refinement or aggregation phase. The
// entry point keeps the historical `leiden` name for API stability.

use rand::SeedableRng as _;
use rand::seq::SliceRandom as _;

/// Undirected weighted graph representation.
pub struct Graph {
    pub n: usize,
    /// Adjacency list: node -> [(neighbor_index, edge_weight)]
    pub neighbors: Vec<Vec<(usize, f64)>>,
    /// Sum of all edge weights (each undirected edge counted once).
    pub total_weight: f64,
}

/// A detected community of nodes.
#[derive(Debug, Clone)]
pub struct Community {
    pub id: u32,
    pub members: Vec<usize>,
    /// internal_weight / total_weight for member nodes
    pub cohesion: f64,
}

/// Result returned by [`leiden`].
#[derive(Debug, Clone)]
pub struct ClusteringResult {
    /// Maps node index to community id.
    pub assignment: Vec<u32>,
    pub communities: Vec<Community>,
    pub modularity: f64,
}

/// Newman–Girvan modularity over ALL intra-community pairs:
///
///   Q = Σ_c [ l_c/m − (d_c/2m)² ]
///
/// where `m` is the total edge weight, `l_c` the internal edge weight of
/// community c (each undirected edge counted once), and `d_c` the sum of
/// weighted degrees of c's members. The degree-product term applies to every
/// pair of nodes in a community — not just adjacent ones — which is why the
/// per-community closed form is used. An all-in-one community yields Q = 0.
///
/// Returns 0.0 when total_weight is 0.
pub fn modularity(graph: &Graph, assignment: &[u32]) -> f64 {
    let m = graph.total_weight;
    if m == 0.0 {
        return 0.0;
    }

    // Per-community internal edge weight (adjacency counts each undirected
    // edge twice, once per direction) and summed weighted degree.
    let mut internal: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
    let mut degree_sum: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
    for i in 0..graph.n {
        let c = assignment[i];
        let deg_i: f64 = graph.neighbors[i].iter().map(|(_, w)| w).sum();
        *degree_sum.entry(c).or_insert(0.0) += deg_i;
        for (j, a_ij) in &graph.neighbors[i] {
            if assignment[*j] == c {
                *internal.entry(c).or_insert(0.0) += a_ij;
            }
        }
    }

    let mut q = 0.0_f64;
    for (c, d_c) in &degree_sum {
        let l_c = internal.get(c).copied().unwrap_or(0.0) / 2.0;
        q += l_c / m - (d_c / (2.0 * m)).powi(2);
    }
    q
}

/// Louvain-style local-moving community detection (single-level; no Leiden
/// refinement/aggregation phases — see the module note).
///
/// - `resolution` controls community size (typical range 0.5–2.0; 1.0 = standard modularity).
///   Non-finite or non-positive values are clamped to 1.0 so a garbage CLI/MCP
///   argument degrades to the standard partition instead of a nonsense one.
/// - `max_iterations` caps the local-moving phase.
pub fn leiden(graph: &Graph, resolution: f64, max_iterations: u32) -> ClusteringResult {
    let resolution = if resolution.is_finite() && resolution > 0.0 {
        resolution
    } else {
        1.0
    };
    if graph.n == 0 || graph.total_weight <= 0.0 {
        // No nodes or no edges — each node is its own singleton community.
        // Without edges, the delta-Q formula would divide by zero
        // (2 * total_weight), so bail out early.
        let assignment: Vec<u32> = (0..graph.n as u32).collect();
        let communities: Vec<Community> = (0..graph.n)
            .map(|i| Community {
                id: i as u32,
                members: vec![i],
                cohesion: 0.0,
            })
            .collect();
        return ClusteringResult {
            assignment,
            communities,
            modularity: 0.0,
        };
    }

    // Weighted degree of each node.
    let degrees: Vec<f64> = (0..graph.n)
        .map(|i| graph.neighbors[i].iter().map(|(_, w)| w).sum())
        .collect();

    // Start: every node in its own community.
    let mut assignment: Vec<u32> = (0..graph.n as u32).collect();

    // sigma[c] = sum of degrees of all nodes in community c.
    let mut sigma: Vec<f64> = degrees.clone();

    // Seed the RNG with a fixed constant so the shuffled visit order — and thus
    // the community labels and total count — are identical across processes. An
    // unseeded `rand::rng()` reseeds from OS entropy every run, which made a
    // handful of borderline nodes land in different communities and drift the
    // reported cluster_id / cluster_count between calls on the same graph.
    let mut rng = rand::rngs::StdRng::seed_from_u64(0x_4E57_C105); // "NW CLuS"
    let mut order: Vec<usize> = (0..graph.n).collect();

    for _ in 0..max_iterations {
        order.shuffle(&mut rng);
        let mut moved = false;

        for &i in &order {
            let c_old = assignment[i] as usize;

            // Compute w_in for each neighbouring community (including c_old).
            // w_in_to[c] = sum of weights from i to nodes in community c.
            // BTreeMap (not HashMap) so `keys()` below yields candidate
            // communities in a deterministic ascending order — equal-`delta`
            // ties then resolve to the lowest community index every run instead
            // of depending on per-process HashMap iteration order.
            let mut w_in_to: std::collections::BTreeMap<usize, f64> =
                std::collections::BTreeMap::new();
            for (j, w) in &graph.neighbors[i] {
                let cj = assignment[*j] as usize;
                *w_in_to.entry(cj).or_insert(0.0) += w;
            }

            let w_in_old = *w_in_to.get(&c_old).unwrap_or(&0.0);

            let mut best_c = c_old;
            let mut best_delta = 0.0_f64;

            // Try each neighboring community.
            let candidate_communities: Vec<usize> = w_in_to.keys().copied().collect();
            for c_new in candidate_communities {
                if c_new == c_old {
                    continue;
                }
                let w_in_new = *w_in_to.get(&c_new).unwrap_or(&0.0);

                // delta_Q for moving i from c_old -> c_new.
                let delta = (w_in_new - w_in_old)
                    + resolution * degrees[i] * (sigma[c_old] - degrees[i] - sigma[c_new])
                        / (2.0 * graph.total_weight);

                if delta > best_delta {
                    best_delta = delta;
                    best_c = c_new;
                }
            }

            if best_c != c_old {
                sigma[c_old] -= degrees[i];
                sigma[best_c] += degrees[i];
                assignment[i] = best_c as u32;
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }

    // Compact community IDs to contiguous 0..k.
    let mut id_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut next_id = 0u32;
    let mut compacted: Vec<u32> = vec![0; graph.n];
    for i in 0..graph.n {
        let old_id = assignment[i];
        let new_id = id_map.entry(old_id).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        compacted[i] = *new_id;
    }

    // Build community member lists.
    let k = next_id as usize;
    let mut members: Vec<Vec<usize>> = vec![vec![]; k];
    for (i, &cid) in compacted.iter().enumerate() {
        members[cid as usize].push(i);
    }

    // Build Community structs with cohesion.
    let communities: Vec<Community> = members
        .into_iter()
        .enumerate()
        .map(|(id, m)| {
            let cohesion = compute_cohesion(graph, &m);
            Community {
                id: id as u32,
                members: m,
                cohesion,
            }
        })
        .collect();

    let q = modularity(graph, &compacted);

    ClusteringResult {
        assignment: compacted,
        communities,
        // Guard against NaN/Infinity leaking into the result (can happen
        // when total_weight is 0 with isolated nodes). JSON serialisation
        // rejects non-finite f64 so this prevents a downstream error.
        modularity: if q.is_finite() { q } else { 0.0 },
    }
}

/// Internal cohesion of a set of nodes: internal_weight / total_weight.
///
/// `internal_weight` = sum of edge weights where both endpoints are in `members`.
/// `total_weight`    = sum of all edge weights incident on any member.
fn compute_cohesion(graph: &Graph, members: &[usize]) -> f64 {
    if members.is_empty() {
        return 0.0;
    }
    let member_set: std::collections::HashSet<usize> = members.iter().copied().collect();

    let mut internal_weight = 0.0_f64;
    let mut total_weight = 0.0_f64;

    for &i in members {
        for (j, w) in &graph.neighbors[i] {
            total_weight += w;
            if member_set.contains(j) {
                internal_weight += w;
            }
        }
    }

    if total_weight == 0.0 {
        return 0.0;
    }
    let c = internal_weight / total_weight;
    if c.is_finite() { c } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a triangle graph: 0-1, 1-2, 0-2 with given weight.
    fn triangle(weight: f64) -> Graph {
        let neighbors = vec![
            vec![(1, weight), (2, weight)],
            vec![(0, weight), (2, weight)],
            vec![(0, weight), (1, weight)],
        ];
        Graph {
            n: 3,
            neighbors,
            total_weight: 3.0 * weight,
        }
    }

    #[test]
    fn single_community_has_zero_modularity() {
        // True Newman–Girvan Q puts ALL nodes in one community at
        // Q = 0 (l_c = m and d_c = 2m, so m/m − (2m/2m)² = 0). The old
        // adjacent-pairs-only computation wrongly returned a positive value.
        let graph = triangle(1.0);
        let assignment = vec![0u32; 3];
        let q = modularity(&graph, &assignment);
        assert!(
            q.abs() < 1e-12,
            "expected ~0 modularity for all-in-one community, got {q}"
        );
    }

    #[test]
    fn modularity_matches_newman_girvan_closed_form() {
        // Two triangles joined by a weak bridge (0.1), partitioned along the
        // bridge: m = 6.1, each community has l_c = 3.0 and d_c = 6.1, so
        // Q = 2 * (3/6.1 − (6.1/12.2)²) ≈ 0.4836.
        let neighbors = vec![
            vec![(1, 1.0), (2, 1.0)],
            vec![(0, 1.0), (2, 1.0)],
            vec![(0, 1.0), (1, 1.0), (3, 0.1)],
            vec![(2, 0.1), (4, 1.0), (5, 1.0)],
            vec![(3, 1.0), (5, 1.0)],
            vec![(3, 1.0), (4, 1.0)],
        ];
        let raw: f64 = neighbors
            .iter()
            .flat_map(|v| v.iter().map(|(_, w)| w))
            .sum();
        let graph = Graph {
            n: 6,
            neighbors,
            total_weight: raw / 2.0,
        };
        let assignment = vec![0u32, 0, 0, 1, 1, 1];
        let q = modularity(&graph, &assignment);
        let expected = 2.0 * (3.0 / 6.1 - (6.1 / 12.2_f64).powi(2));
        assert!(
            (q - expected).abs() < 1e-12,
            "expected Q ≈ {expected}, got {q}"
        );
    }

    #[test]
    fn leiden_clamps_invalid_resolution() {
        // clusters --resolution NaN/inf/-1 must degrade to the standard
        // (resolution = 1.0) partition instead of producing garbage.
        let graph = triangle(1.0);
        let baseline = leiden(&graph, 1.0, 100);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0] {
            let result = leiden(&graph, bad, 100);
            assert_eq!(
                result.assignment, baseline.assignment,
                "resolution {bad} must clamp to 1.0"
            );
            assert!(result.modularity.is_finite());
        }
    }

    #[test]
    fn each_own_community_has_zero_modularity() {
        let graph = triangle(1.0);
        let assignment = vec![0u32, 1, 2];
        let q = modularity(&graph, &assignment);
        assert!(
            q <= 0.0,
            "expected non-positive modularity for each-in-own community, got {q}"
        );
    }

    #[test]
    fn leiden_finds_two_clusters() {
        // Two triangles (0-1-2 and 3-4-5) connected by a weak bridge 2-3.
        let neighbors = vec![
            vec![(1, 1.0), (2, 1.0)],           // 0
            vec![(0, 1.0), (2, 1.0)],           // 1
            vec![(0, 1.0), (1, 1.0), (3, 0.1)], // 2
            vec![(2, 0.1), (4, 1.0), (5, 1.0)], // 3
            vec![(3, 1.0), (5, 1.0)],           // 4
            vec![(3, 1.0), (4, 1.0)],           // 5
        ];
        // total_weight: 3 + 3 + 0.1 = 6.1 (each undirected edge counted once here we count per
        // direction, so bridge adds 0.1*2 = 0.2; triangles 3*2=6 edges each side -> 6.0+0.2=6.2)
        // Actually count by summing all neighbor weights and dividing by 2.
        let raw: f64 = neighbors
            .iter()
            .flat_map(|v| v.iter().map(|(_, w)| w))
            .sum();
        let graph = Graph {
            n: 6,
            neighbors,
            total_weight: raw / 2.0,
        };

        let result = leiden(&graph, 1.0, 100);

        assert_eq!(result.assignment.len(), 6);

        // Group 0-1-2 should be in the same community.
        assert_eq!(
            result.assignment[0], result.assignment[1],
            "0 and 1 should cluster together"
        );
        assert_eq!(
            result.assignment[1], result.assignment[2],
            "1 and 2 should cluster together"
        );

        // Group 3-4-5 should be in the same community.
        assert_eq!(
            result.assignment[3], result.assignment[4],
            "3 and 4 should cluster together"
        );
        assert_eq!(
            result.assignment[4], result.assignment[5],
            "4 and 5 should cluster together"
        );

        // The two groups must differ.
        assert_ne!(
            result.assignment[0], result.assignment[3],
            "the two triangles should be in different communities"
        );

        assert!(
            result.modularity > 0.3,
            "expected modularity > 0.3, got {}",
            result.modularity
        );
    }

    #[test]
    fn leiden_handles_empty_graph() {
        let graph = Graph {
            n: 0,
            neighbors: vec![],
            total_weight: 0.0,
        };
        let result = leiden(&graph, 1.0, 10);
        assert!(result.assignment.is_empty());
        assert!(result.communities.is_empty());
    }

    #[test]
    fn leiden_handles_disconnected_nodes() {
        // 3 nodes, no edges.
        let graph = Graph {
            n: 3,
            neighbors: vec![vec![], vec![], vec![]],
            total_weight: 0.0,
        };
        let result = leiden(&graph, 1.0, 10);
        assert_eq!(result.assignment.len(), 3);
        // Each node should be in its own community (no edges to attract them).
        assert_ne!(result.assignment[0], result.assignment[1]);
        assert_ne!(result.assignment[1], result.assignment[2]);
        assert_ne!(result.assignment[0], result.assignment[2]);
        assert_eq!(result.communities.len(), 3);
    }

    #[test]
    fn leiden_low_resolution_produces_finite_values() {
        // Two triangles connected by a weak bridge.
        let neighbors = vec![
            vec![(1, 1.0), (2, 1.0)],
            vec![(0, 1.0), (2, 1.0)],
            vec![(0, 1.0), (1, 1.0), (3, 0.1)],
            vec![(2, 0.1), (4, 1.0), (5, 1.0)],
            vec![(3, 1.0), (5, 1.0)],
            vec![(3, 1.0), (4, 1.0)],
        ];
        let graph = Graph {
            n: 6,
            neighbors,
            total_weight: 6.1,
        };
        // Test with different resolution values including the new defaults.
        for &res in &[0.3, 0.5, 1.0, 2.0] {
            let result = leiden(&graph, res, 100);
            assert!(
                result.modularity.is_finite(),
                "modularity should be finite at resolution={res}, got {}",
                result.modularity
            );
            for c in &result.communities {
                assert!(
                    c.cohesion.is_finite(),
                    "cohesion should be finite at resolution={res}, got {}",
                    c.cohesion
                );
            }
        }
    }

    #[test]
    fn leiden_is_deterministic_across_runs() {
        // nw-081 regression: a symmetric graph with several borderline nodes,
        // whose community membership depends on visit order and tie-breaking.
        // Four triangles joined in a ring by equal-weight bridges — the bridge
        // endpoints sit near delta ~= 0, so an unseeded RNG or HashMap-ordered
        // tie-break used to move them differently between runs, drifting both
        // the labels and the total count. Both a fresh `rand::rng()` and a fresh
        // HashMap RandomState differ per call, so an in-process repeat reproduces
        // the old cross-process drift. After the fix, every run is identical.
        // 4 triangles: (0,1,2)(3,4,5)(6,7,8)(9,10,11); ring bridges 2-3,5-6,8-9,11-0.
        let b = 0.3; // equal-weight bridges — deliberately borderline
        let neighbors = vec![
            vec![(1, 1.0), (2, 1.0), (11, b)],  // 0
            vec![(0, 1.0), (2, 1.0)],           // 1
            vec![(0, 1.0), (1, 1.0), (3, b)],   // 2
            vec![(2, b), (4, 1.0), (5, 1.0)],   // 3
            vec![(3, 1.0), (5, 1.0)],           // 4
            vec![(3, 1.0), (4, 1.0), (6, b)],   // 5
            vec![(5, b), (7, 1.0), (8, 1.0)],   // 6
            vec![(6, 1.0), (8, 1.0)],           // 7
            vec![(6, 1.0), (7, 1.0), (9, b)],   // 8
            vec![(8, b), (10, 1.0), (11, 1.0)], // 9
            vec![(9, 1.0), (11, 1.0)],          // 10
            vec![(9, 1.0), (10, 1.0), (0, b)],  // 11
        ];
        let raw: f64 = neighbors
            .iter()
            .flat_map(|v| v.iter().map(|(_, w)| w))
            .sum();
        let make = || Graph {
            n: 12,
            neighbors: neighbors.clone(),
            total_weight: raw / 2.0,
        };

        let first = leiden(&make(), 1.0, 100);
        for run in 0..5 {
            let again = leiden(&make(), 1.0, 100);
            assert_eq!(
                first.assignment, again.assignment,
                "run {run}: per-node cluster labels must be identical across runs"
            );
            assert_eq!(
                first.communities.len(),
                again.communities.len(),
                "run {run}: cluster_count must be identical across runs"
            );
        }
    }
}
