// Leiden community detection for code call graphs.
// Clusters graph nodes into functional communities to enable process-grouped search.

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

/// Newman–Girvan modularity: Q = (1/2m) * Σ_ij [A_ij − k_i*k_j/(2m)] δ(c_i, c_j)
///
/// Returns 0.0 when total_weight is 0.
pub fn modularity(graph: &Graph, assignment: &[u32]) -> f64 {
    let m = graph.total_weight;
    if m == 0.0 {
        return 0.0;
    }

    // Degree (weighted) of each node.
    let degrees: Vec<f64> = (0..graph.n)
        .map(|i| graph.neighbors[i].iter().map(|(_, w)| w).sum())
        .collect();

    let mut q = 0.0_f64;
    for i in 0..graph.n {
        for (j, a_ij) in &graph.neighbors[i] {
            if assignment[i] == assignment[*j] {
                q += a_ij - degrees[i] * degrees[*j] / (2.0 * m);
            }
        }
    }
    q / (2.0 * m)
}

/// Leiden community detection.
///
/// - `resolution` controls community size (typical range 0.5–2.0; 1.0 = standard modularity).
/// - `max_iterations` caps the local-moving phase.
pub fn leiden(graph: &Graph, resolution: f64, max_iterations: u32) -> ClusteringResult {
    if graph.n == 0 {
        return ClusteringResult {
            assignment: vec![],
            communities: vec![],
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

    let mut rng = rand::rng();
    let mut order: Vec<usize> = (0..graph.n).collect();

    for _ in 0..max_iterations {
        order.shuffle(&mut rng);
        let mut moved = false;

        for &i in &order {
            let c_old = assignment[i] as usize;

            // Compute w_in for each neighbouring community (including c_old).
            // w_in_to[c] = sum of weights from i to nodes in community c.
            let mut w_in_to: std::collections::HashMap<usize, f64> =
                std::collections::HashMap::new();
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
        modularity: q,
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
        0.0
    } else {
        internal_weight / total_weight
    }
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
    fn single_community_has_positive_modularity() {
        let graph = triangle(1.0);
        let assignment = vec![0u32; 3];
        let q = modularity(&graph, &assignment);
        assert!(
            q > 0.0,
            "expected positive modularity for all-in-one community, got {q}"
        );
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
}
