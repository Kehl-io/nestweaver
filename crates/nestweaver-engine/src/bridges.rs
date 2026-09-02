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
/// Discards everything a caller needs to describe the answer honestly. Prefer
/// [`find_bridge_nodes_bounded`] on any surface that publishes a `total`, a
/// `truncated`, or the score itself; this wrapper exists for the callers that
/// publish none of them.
pub fn find_bridge_nodes(store: &GraphStore, top_n: usize) -> Result<Vec<BridgeNode>> {
    find_bridge_nodes_bounded(store, top_n).map(|found| found.bridges)
}

/// The top-N bridges, the population they were selected FROM, and how the
/// score in them was actually computed.
///
/// nw-398. Both halves of this struct are honesty fixes, and they point in
/// OPPOSITE directions. `candidate_total` exists because the cut was invisible:
/// `bridges --top 3` reported neither a total nor a `truncated`, and the MCP
/// twin's `count` was `len(list)` by construction. `sources_sampled` /
/// `sampled` exist because the score was invisibly APPROXIMATE: a
/// `SAMPLE_LIMIT`-source estimate rendered at full `f64` precision
/// (`18866919.81139078`) with no field a program could branch on. The tool
/// description said "approximate for large graphs" — prose a human reads.
pub struct BridgeNodes {
    pub bridges: Vec<BridgeNode>,
    /// How many symbols were CANDIDATES — i.e. had at least one code edge.
    /// A symbol with no edges lies on no path between any pair and therefore
    /// cannot be a bridge at any `top_n`, so counting it would overstate the
    /// population. Same definition, and the same argument, as
    /// [`crate::hubs::HubNodes::candidate_total`].
    pub candidate_total: usize,
    /// How many BFS sources the betweenness computation actually ran from.
    /// Equal to the node count when the graph is small enough to do exactly.
    pub sources_sampled: usize,
    /// True when `sources_sampled` was a SAMPLE rather than every node — i.e.
    /// every `betweenness_score` in `bridges` is an estimate. This is the
    /// field a program should branch on; the digits in the score itself carry
    /// no information about it.
    pub sampled: bool,
}

impl BridgeNodes {
    /// Whether `top_n` cut the population — the one definition, so a consumer
    /// cannot re-derive it from the already-cut list and get `false` for free.
    pub fn truncated(&self) -> bool {
        self.candidate_total > self.bridges.len()
    }
}

/// [`find_bridge_nodes`] with the candidate count and the sampling provenance
/// retained.
///
/// Uses Brandes' algorithm over a bounded set of BFS sources: every node when
/// the graph has at most `SAMPLE_LIMIT` of them, otherwise `SAMPLE_LIMIT`
/// sources chosen by even spacing over graph INSERTION ORDER. That spacing is
/// deterministic, NOT random, so the resulting bias is systematic and
/// reproducible rather than self-cancelling across calls — which is precisely
/// why the estimate has to be labelled as one rather than left to average out.
pub fn find_bridge_nodes_bounded(store: &GraphStore, top_n: usize) -> Result<BridgeNodes> {
    let graph = match load_graph(store)? {
        Some(g) => g,
        None => {
            return Ok(BridgeNodes {
                bridges: vec![],
                candidate_total: 0,
                sources_sampled: 0,
                sampled: false,
            });
        }
    };

    let n = graph.symbols.len();

    // Counted from the adjacency rather than the ranked list, and BEFORE the
    // truncate below, so the population is reported even when `top_n` is 0 and
    // nothing survives selection.
    let candidate_total = graph.adj.iter().filter(|nbrs| !nbrs.is_empty()).count();

    // Compute betweenness centrality via Brandes' algorithm with sampling.
    let Betweenness {
        scores: betweenness,
        sources_sampled,
    } = brandes_sampled(&graph.adj, n);

    // Build bridge nodes.
    //
    // ZERO-DEGREE SYMBOLS ARE EXCLUDED, and this is a correctness fix rather
    // than a tidy-up. `candidate_total` above counts symbols with at least one
    // edge, but this list was built from EVERY symbol. A zero-degree symbol has
    // betweenness 0.0 by definition — no path can run through it — so it TIES
    // with every edge-bearing leaf, and `sort_by` is stable, so ties resolve by
    // graph insertion order. A zero-degree symbol could therefore take a slot
    // from a real candidate.
    //
    // That made `truncated()` LIE in the safe-looking direction: with symbols
    // inserted `lone, s0, s1` and one edge `s0—s1`, `candidate_total` is 2, and
    // `top_n = 2` returns `[lone, s0]` — `s1`, a candidate, was cut — while
    // `truncated()` computed `2 > 2 == false` and published a cut answer as
    // complete. It also let `returned` exceed `total`, which is not a ratio any
    // consumer can use.
    //
    // `hubs` never had this because its heap key is degree-first, so a
    // non-candidate can never outrank a candidate. Filtering here gives bridges
    // the same property the two structs' shared docblock already claims.
    let mut bridges: Vec<BridgeNode> = graph
        .symbols
        .iter()
        .enumerate()
        .filter(|(i, _)| !graph.adj[*i].is_empty())
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
    Ok(BridgeNodes {
        bridges,
        candidate_total,
        // A sample only when fewer sources ran than the graph has nodes; on a
        // graph of at most SAMPLE_LIMIT nodes every node is a source and the
        // result is exact, which callers must be able to say too.
        sampled: sources_sampled < n,
        sources_sampled,
    })
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

/// Betweenness scores plus the fact that says how much to trust them.
struct Betweenness {
    scores: Vec<f64>,
    /// How many BFS sources were actually run. `< n` means the scores are an
    /// extrapolated estimate, not an exact count.
    sources_sampled: usize,
}

/// Brandes' algorithm for betweenness centrality with source sampling.
///
/// For each source node s (up to SAMPLE_LIMIT), runs BFS to compute:
/// - sigma[t]: number of shortest paths from s to t
/// - delta[v]: dependency of s on v
///
/// The betweenness of each node v is the sum of delta[v] across all sources,
/// then scaled — see the scaling block at the end for what that scale IS.
///
/// nw-398: this doc used to claim the result was "normalized by the number of
/// sources sampled". It is not, and never was: on the sampled path the scale
/// factor is `n / (2 * num_sources)`, which SCALES UP to estimate the
/// full-graph pair count. Dividing by the source count would have produced a
/// per-source average, a different quantity an order of magnitude smaller. The
/// claim was removed rather than implemented because the unnormalized value is
/// what every existing caller ranks and renders; what was missing was the
/// label, not the division.
fn brandes_sampled(adj: &[Vec<usize>], n: usize) -> Betweenness {
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

    // Scale the accumulated dependencies. For an undirected graph each pair is
    // counted twice in the BFS, hence the factor of 2 in both branches. This is
    // NOT a normalization to any unit interval, and on the sampled branch it is
    // an extrapolation, not a division by the sample size (nw-398).
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

    Betweenness {
        scores: betweenness,
        sources_sampled: num_sources,
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
            end_line: 1,
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
            canonical_id: None,
        }
    }

    fn make_edge(src: &str, tgt: &str) -> ResolvedEdge {
        ResolvedEdge {
            source_uid: src.to_string(),
            target_uid: tgt.to_string(),
            edge_type: EdgeType::Calls,
            confidence: 1.0,
            link_type: None,
            evidence: vec![],
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

    /// A chain of `chain` edge-bearing symbols plus `isolated` symbols with no
    /// edges at all. The isolated ones are the counterweight's material: they
    /// pad the returned list without enlarging the candidate population.
    fn chain_store(chain: usize, isolated: usize) -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        for i in 0..chain {
            store
                .insert_symbol(&make_symbol(
                    &format!("s{i}"),
                    &format!("fn_{i}"),
                    "src/lib.rs",
                ))
                .unwrap();
        }
        for i in 0..chain.saturating_sub(1) {
            store
                .insert_edge(&make_edge(&format!("s{i}"), &format!("s{}", i + 1)))
                .unwrap();
        }
        for i in 0..isolated {
            store
                .insert_symbol(&make_symbol(
                    &format!("lone{i}"),
                    &format!("lone_{i}"),
                    "src/lone.rs",
                ))
                .unwrap();
        }
        store
    }

    /// nw-398. `bridges --top 3 --json` returned `['_meta', 'bridges',
    /// 'rankings_stale', 'stale_repos']` — not even a `count` — and the MCP
    /// twin's `count` was `len(list)`, true by construction. The number that
    /// makes the cut visible has to be the population BEFORE the truncate, and
    /// it was never even computed on this path.
    #[test]
    fn a_cut_bridge_ranking_reports_the_population_it_was_cut_from() {
        let store = chain_store(8, 2);

        let found = find_bridge_nodes_bounded(&store, 3).unwrap();
        assert_eq!(found.bridges.len(), 3);
        assert_eq!(
            found.candidate_total, 8,
            "the population is the edge-bearing symbols, not the returned rows              and not the whole store"
        );
        assert!(found.truncated(), "3 of 8 is a truncation");
    }

    /// COUNTERWEIGHT. A ranking that was NOT cut must say so, including the case
    /// where the caller asked for more than exists.
    ///
    /// UPDATED when the zero-degree filter landed. This previously asserted that
    /// asking for 50 returns all 10 SYMBOLS — the padded tail — and used that to
    /// justify `truncated()` being `>` rather than `!=`. That padding was the
    /// defect: a zero-degree symbol has betweenness 0.0, ties with every real
    /// leaf, and won slots on insertion order, which let a CUT answer report
    /// `truncated == false`. Now the ranking contains only candidates, so asking
    /// for more than exists returns exactly the candidate population and
    /// `returned` can never exceed `total`.
    ///
    /// `truncated()` remains `>` rather than `!=` deliberately: it is now
    /// equivalent for bridges, and keeping the two structs' predicate identical
    /// is worth more than tightening one of them.
    #[test]
    fn an_uncut_bridge_ranking_reports_no_truncation_even_when_asked_for_more() {
        let store = chain_store(8, 2);

        let exact = find_bridge_nodes_bounded(&store, 8).unwrap();
        assert!(
            !exact.truncated(),
            "8 of 8 candidates is complete: {} of {}",
            exact.bridges.len(),
            exact.candidate_total
        );

        let over = find_bridge_nodes_bounded(&store, 50).unwrap();
        assert_eq!(
            over.bridges.len(),
            8,
            "asking for more than exists returns the CANDIDATE population, not every symbol — \
             the two zero-degree symbols are not bridges and never were"
        );
        assert_eq!(over.candidate_total, 8);
        assert!(
            over.bridges.len() <= over.candidate_total,
            "`returned` may never exceed `total`"
        );
        assert!(
            !over.truncated(),
            "asking for 50 and being given everything is not a truncation"
        );

        let empty = find_bridge_nodes_bounded(&GraphStore::in_memory().unwrap(), 5).unwrap();
        assert_eq!(empty.candidate_total, 0);
        assert!(!empty.truncated(), "an empty graph is complete, not cut");
    }

    /// nw-398 leg 2, the honesty defect pointing the OTHER way:
    /// `betweenness_score` is rendered at full `f64` precision
    /// (`18866919.81139078`) whether it was computed exactly or estimated from
    /// at most `SAMPLE_LIMIT` sources. A graph small enough to do exactly must
    /// be able to SAY it was exact, or the flag is useless — every payload
    /// would carry the same warning and consumers would learn to ignore it.
    #[test]
    fn a_betweenness_run_over_every_source_reports_that_it_sampled_nothing() {
        let store = chain_store(10, 0);

        let found = find_bridge_nodes_bounded(&store, 5).unwrap();
        assert_eq!(
            found.sources_sampled, 10,
            "every node is a BFS source below the sample limit"
        );
        assert!(
            !found.sampled,
            "an exact computation must not be labelled a sample"
        );
    }

    /// The other half: past `SAMPLE_LIMIT` the score is an estimate produced
    /// from evenly-spaced sources over INSERTION ORDER — deterministic, so the
    /// bias does not average out across calls — and the payload must carry a
    /// field a program can branch on rather than prose in a tool description.
    #[test]
    fn a_graph_past_the_sample_limit_reports_that_the_score_is_a_sample() {
        let store = chain_store(SAMPLE_LIMIT + 20, 0);

        let found = find_bridge_nodes_bounded(&store, 5).unwrap();
        assert_eq!(
            found.sources_sampled, SAMPLE_LIMIT,
            "the source set is capped at the sample limit"
        );
        assert!(
            found.sampled,
            "a {SAMPLE_LIMIT}-source estimate over {} nodes must be labelled one",
            SAMPLE_LIMIT + 20
        );
    }

    /// nw-398, found by review. `truncated()` must be EXACT, not merely
    /// plausible: it published a CUT answer as complete, because the ranking was
    /// drawn from every symbol while `candidate_total` counted only those with
    /// an edge.
    ///
    /// The fixture is the reviewer's: a zero-degree symbol inserted FIRST, then
    /// a connected pair. Before the fix `lone` tied at 0.0 with the pair's leaf,
    /// won on insertion order because `sort_by` is stable, and `top_n = 2`
    /// returned `[lone, s0]` while reporting `truncated == false`.
    #[test]
    fn a_zero_degree_symbol_cannot_take_a_candidates_slot_and_hide_the_cut() {
        let store = GraphStore::in_memory().unwrap();
        for uid in ["lone", "s0", "s1"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}"), "src/lib.rs"))
                .unwrap();
        }
        store.insert_edge(&make_edge("s0", "s1")).unwrap();

        let found = find_bridge_nodes_bounded(&store, 2).expect("bridges");
        let names: Vec<&str> = found.bridges.iter().map(|b| b.uid.as_str()).collect();
        assert!(
            !names.contains(&"lone"),
            "a zero-degree symbol can never be a bridge — no path runs through it: {names:?}"
        );
        assert_eq!(
            found.candidate_total, 2,
            "only edge-bearing symbols are candidates"
        );
        assert!(
            !found.truncated(),
            "asking for both candidates is a COMPLETE answer, not a truncated one"
        );

        // COUNTERWEIGHT: asking for fewer than the candidate population must
        // still report the cut. Without it, excluding everything would satisfy
        // the assertion above.
        let cut = find_bridge_nodes_bounded(&store, 1).expect("bridges");
        assert!(
            cut.truncated(),
            "one of two candidates is a cut and must say so"
        );
        assert!(
            cut.bridges.len() <= cut.candidate_total,
            "`returned` may never exceed `total` — that ratio is meaningless to a consumer"
        );
    }
}
