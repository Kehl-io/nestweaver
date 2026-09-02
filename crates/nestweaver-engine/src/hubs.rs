//! Hub node detection: finds the most connected nodes in the code graph.
//!
//! A hub is a symbol with high degree centrality (many incoming + outgoing
//! edges) and/or high PageRank. Hubs represent central abstractions that
//! many parts of the codebase depend on.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

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
///
/// nw-398: this wrapper DISCARDS `candidate_total`, so a caller that publishes
/// a `total` or a `truncated` cannot use it — `hubs`/`brain_hub_nodes` did,
/// and reported `count == len(list)`, which is true by construction and
/// therefore says nothing. Use [`find_hub_nodes_bounded`] on any surface that
/// makes a claim about completeness.
pub fn find_hub_nodes(store: &GraphStore, top_n: usize) -> Result<Vec<HubNode>> {
    find_hub_nodes_bounded(store, top_n).map(|found| found.hubs)
}

/// The top-N hubs plus the size of the population they were selected FROM.
pub struct HubNodes {
    pub hubs: Vec<HubNode>,
    /// How many symbols were CANDIDATES — i.e. had at least one code edge.
    /// A symbol with no edges cannot be a hub at any `top_n`, so counting it
    /// would overstate the population in the other direction.
    pub candidate_total: usize,
}

impl HubNodes {
    /// Whether `top_n` cut the candidate population.
    ///
    /// The one definition, so the three surfaces that publish it cannot each
    /// re-derive it from the already-cut list and each get `false` for free.
    ///
    /// Deliberately `>` and not `!=`: when the caller asks for MORE than the
    /// graph has, the heap pads the tail with zero-degree symbols, so
    /// `hubs.len()` can exceed `candidate_total`. That is a caller who was
    /// given everything, which is the opposite of truncation.
    pub fn truncated(&self) -> bool {
        self.candidate_total > self.hubs.len()
    }
}

/// [`find_hub_nodes`] with the candidate count retained.
///
/// `top_n` is honest when the CALLER chose it (`hubs --top 30` asked for 30).
/// It is not honest when an internal constant chose it: `summary --level hub`
/// hard-codes 30 and reported `{returned: 30, total: 30, truncated: false}` on
/// a 180-candidate graph — the same shape as F-DC-11 one level up, and found
/// by asking where else that property had to hold rather than by a report.
pub fn find_hub_nodes_bounded(store: &GraphStore, top_n: usize) -> Result<HubNodes> {
    let (symbols, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!(e))
        .context("failed to load graph data for hub detection")?;

    if symbols.is_empty() {
        return Ok(HubNodes {
            hubs: vec![],
            candidate_total: 0,
        });
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

    // Read PageRank scores from the in-memory cache. Fails closed during a
    // dirty index publication (the ranking.rs module contract): hub ranking
    // must not silently treat every symbol as score 0.
    let pr_scores: HashMap<String, f64> =
        store.pagerank_scores().map_err(|e| anyhow::anyhow!(e))?;

    // Feature F12: when git-activity recency scores are loaded, demote dormant
    // code at read time. We apply the same clamped multiplier the store uses in
    // `symbols_by_pagerank`, keyed by the symbol's file path. Files with no
    // recency score → neutral (multiplier 1.0); when no cache is loaded, the
    // multiplier is 1.0 for every file (no-op).
    let ga_active = store.has_git_activity();
    let ga_weight = store.git_activity_weight();

    // nw-179: select the top N with a bounded heap instead of materializing and
    // sorting the whole corpus. The previous shape built a HubNode -- three
    // String clones each -- for every symbol (~580k allocations at 193k
    // symbols), sorted all of them, then threw away all but `top_n`. Cost was
    // independent of what the caller asked for: `--top 10` and `--top 1000`
    // both took ~5s. Now only the survivors are materialized, so the work is
    // O(n log k) with k allocations rather than O(n log n) with n.
    // Counted BEFORE the `top_n == 0` short-circuit and before the heap, so
    // the population is reported even when nothing survives selection.
    let candidate_total = (0..n).filter(|&i| in_degree[i] + out_degree[i] > 0).count();

    if top_n == 0 {
        return Ok(HubNodes {
            hubs: vec![],
            candidate_total,
        });
    }

    /// Sort key for one candidate: degree first, PageRank as the tie-break,
    /// with the symbol index last so the ordering is total and stable.
    #[derive(PartialEq)]
    struct HubKey {
        total_degree: usize,
        pagerank: f64,
        index: usize,
    }
    impl Eq for HubKey {}
    impl Ord for HubKey {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.total_degree
                .cmp(&other.total_degree)
                // f64 is not Ord; total_cmp gives a total order without
                // unwrap_or(Equal) silently collapsing NaN into ties.
                .then_with(|| self.pagerank.total_cmp(&other.pagerank))
                // Lower index wins a full tie, matching the previous stable sort.
                .then_with(|| other.index.cmp(&self.index))
        }
    }
    impl PartialOrd for HubKey {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    // Min-heap of the best `top_n` seen so far: the weakest survivor sits on
    // top, so admitting a candidate is one comparison.
    let mut best: BinaryHeap<Reverse<HubKey>> = BinaryHeap::with_capacity(top_n);
    for (index, sym) in symbols.iter().enumerate() {
        let base = pr_scores.get(&sym.uid).copied().unwrap_or(0.0);
        let pagerank = if ga_active {
            base * nestweaver_store::git_activity_multiplier(
                // Repo-keyed for the same reason as the ranking path: a
                // repo-relative path alone matches the wrong repo (nw-233).
                store.git_activity_score(&sym.repo_uid, &sym.file_path),
                ga_weight,
            )
        } else {
            base
        };
        let key = HubKey {
            total_degree: in_degree[index] + out_degree[index],
            pagerank,
            index,
        };
        if best.len() < top_n {
            best.push(Reverse(key));
        } else if best.peek().is_some_and(|weakest| key > weakest.0) {
            best.pop();
            best.push(Reverse(key));
        }
    }

    // Drain into descending order, then materialize strings for survivors only.
    let mut ranked: Vec<HubKey> = best.into_iter().map(|Reverse(key)| key).collect();
    ranked.sort_by(|a, b| b.cmp(a));
    let hubs: Vec<HubNode> = ranked
        .into_iter()
        .map(|key| {
            let sym = &symbols[key.index];
            HubNode {
                uid: sym.uid.clone(),
                name: sym.name.clone(),
                file_path: sym.file_path.clone(),
                in_degree: in_degree[key.index],
                out_degree: out_degree[key.index],
                total_degree: key.total_degree,
                pagerank_score: key.pagerank,
                cluster_id: None,
            }
        })
        .collect();

    Ok(HubNodes {
        hubs,
        candidate_total,
    })
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

    /// nw-179: the bounded heap must select the SAME set, in the same order, as
    /// sorting everything and truncating. A min-heap with the comparison the
    /// wrong way round silently returns the WEAKEST n, which every existing
    /// test above would still pass.
    #[test]
    fn bounded_selection_matches_a_full_sort() {
        let store = GraphStore::in_memory().unwrap();
        // 12 symbols with deliberately uneven, non-monotonic degrees.
        for i in 0..12 {
            store
                .insert_symbol(&make_symbol(
                    &format!("s{i}"),
                    &format!("sym{i}"),
                    "src/a.rs",
                ))
                .unwrap();
        }
        // Give s3 the most edges, then s7, then s1 — deliberately not in uid order.
        let plan: &[(&str, usize)] = &[("s3", 5), ("s7", 4), ("s1", 3), ("s9", 2), ("s5", 1)];
        for (target, count) in plan {
            for n in 0..*count {
                let src = format!("s{}", (n + 10) % 12);
                store.insert_edge(&make_edge(&src, target)).unwrap();
            }
        }

        let all = find_hub_nodes(&store, 12).unwrap();
        let expected: Vec<&str> = all.iter().take(3).map(|h| h.uid.as_str()).collect();
        let top3 = find_hub_nodes(&store, 3).unwrap();
        let actual: Vec<&str> = top3.iter().map(|h| h.uid.as_str()).collect();
        assert_eq!(
            actual, expected,
            "top-3 must equal the first 3 of the full ranking"
        );
        assert!(
            top3[0].total_degree >= top3[1].total_degree,
            "results must be in descending degree order"
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
    fn git_activity_demotes_dormant_hub_pagerank() {
        // Feature F12: two equally-connected symbols in different files. When
        // git-activity marks one file dormant, its hub pagerank_score is
        // demoted relative to the live one (degree-based ordering is unchanged,
        // but the reported pagerank reflects the recency multiplier).
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("F", "fn_f", "src/fresh.rs"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("S", "fn_s", "src/stale.rs"))
            .unwrap();
        store.insert_edge(&make_edge("F", "S")).unwrap();
        store.insert_edge(&make_edge("S", "F")).unwrap();
        store
            .compute_pagerank(0.85, 30, &nestweaver_store::GraphScope::code_only())
            .unwrap();

        // Repo-keyed now (nw-233): the fixture's symbols all belong to
        // "repo-1", and a score filed under any other repo must not reach them.
        let mut paths = HashMap::new();
        paths.insert("src/fresh.rs".to_string(), 0.95);
        paths.insert("src/stale.rs".to_string(), 0.05);
        let mut ga = HashMap::new();
        ga.insert("repo-1".to_string(), paths);
        store.load_git_activity_cache(ga);

        let hubs = find_hub_nodes(&store, 10).unwrap();
        let f = hubs.iter().find(|h| h.uid == "F").unwrap();
        let s = hubs.iter().find(|h| h.uid == "S").unwrap();
        assert!(
            f.pagerank_score > s.pagerank_score,
            "fresh hub ({:.6}) should outrank dormant hub ({:.6})",
            f.pagerank_score,
            s.pagerank_score
        );
    }

    /// nw-398. `find_hub_nodes_bounded` has computed `candidate_total` since
    /// nw-299, and every caller took the discarding wrapper, so the number was
    /// computed and thrown away on every call. This pins the reachable shape:
    /// the population, and a `truncated` derived from it exactly once.
    #[test]
    fn a_cut_hub_ranking_reports_the_population_it_was_cut_from() {
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
        // Two symbols with no edges at all: they can never be hubs, so they
        // are not candidates, but the heap will still pad the tail with them
        // once the caller asks for more rows than there are candidates.
        for i in 0..2 {
            store
                .insert_symbol(&make_symbol(
                    &format!("lone{i}"),
                    &format!("lone_{i}"),
                    "src/lone.rs",
                ))
                .unwrap();
        }

        let found = find_hub_nodes_bounded(&store, 3).unwrap();
        assert_eq!(found.hubs.len(), 3);
        assert_eq!(
            found.candidate_total, 10,
            "the population is the edge-bearing symbols, not the 12 in the store"
        );
        assert!(found.truncated(), "3 of 10 is a truncation");
    }

    /// COUNTERWEIGHT: a ranking that took everything must report
    /// `truncated == false`, including when the caller asked for MORE than the
    /// graph has and got zero-degree padding back — `hubs.len()` then exceeds
    /// `candidate_total`, and a `!=` would call a complete answer truncated.
    #[test]
    fn an_uncut_hub_ranking_reports_no_truncation_even_when_asked_for_more() {
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
        for i in 0..2 {
            store
                .insert_symbol(&make_symbol(
                    &format!("lone{i}"),
                    &format!("lone_{i}"),
                    "src/lone.rs",
                ))
                .unwrap();
        }

        let exact = find_hub_nodes_bounded(&store, 10).unwrap();
        assert!(
            !exact.truncated(),
            "10 of 10 candidates is complete: {} of {}",
            exact.hubs.len(),
            exact.candidate_total
        );

        let over = find_hub_nodes_bounded(&store, 50).unwrap();
        assert_eq!(over.hubs.len(), 12, "every symbol is returned");
        assert_eq!(over.candidate_total, 10);
        assert!(
            !over.truncated(),
            "asking for 50 and being given everything is not a truncation"
        );

        let empty = find_hub_nodes_bounded(&GraphStore::in_memory().unwrap(), 5).unwrap();
        assert_eq!(empty.candidate_total, 0);
        assert!(!empty.truncated(), "an empty graph is complete, not cut");
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
