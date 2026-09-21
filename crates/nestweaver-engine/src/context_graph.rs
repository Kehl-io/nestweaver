//! Context presentation: real edges, explicit visibility, bounded response.
use std::collections::HashSet;
use std::time::{Duration, Instant};

use nestweaver_store::context_graph::{CONTEXT_EDGE_LIMIT, CONTEXT_NODE_LIMIT, ContextEdge};
use nestweaver_store::{GraphStore, StoreError};
use serde::Serialize;

use crate::BrainContextResult;
use crate::authz::{VisibleRepos, repo_is_visible};

#[derive(Debug, Serialize)]
pub struct ContextGraphMeta {
    pub generation: u64,
    pub node_limit: usize,
    pub edge_limit: usize,
    pub omitted_nodes: usize,
    pub omitted_edges: usize,
    pub edge_count_relation: &'static str,
    pub truncated: bool,
    pub edge_scope: &'static str,
}

pub struct ContextGraph {
    pub edges: Vec<ContextEdge>,
    pub meta: ContextGraphMeta,
}

/// The route captures `generation` before selecting its context and calls this
/// after workspace filtering. Never publish a mix of old nodes and new edges.
///
/// Publication uses the same Q7 predicate as ranked MCP/CLI reads
/// ([`GraphStore::index_publication_blocks_ranking`]): a non-wedged
/// `brain watcher batch` marker does **not** fail closed. A full `index`
/// publication, an unattributed marker, or a generation change still
/// refuses — HTTP used to call [`GraphStore::is_index_publication_dirty`]
/// here, which 503'd the UI for the entire watcher-batch window while
/// `brain_context` over MCP/CLI answered.
pub fn ensure_context_generation(store: &GraphStore, generation: u64) -> anyhow::Result<()> {
    if store.index_publication_blocks_ranking() || store.graph_generation() != generation {
        return Err(StoreError::RankingUnavailable.into());
    }
    Ok(())
}

pub fn attach_context_graph(
    store: &GraphStore,
    result: &mut BrainContextResult,
    visible: &VisibleRepos,
    generation: u64,
) -> anyhow::Result<ContextGraph> {
    ensure_context_generation(store, generation)?;
    // Visibility precedes caps and counts. Missing/unknown ownership does not
    // inherit VisibleRepos::allows's special blast-radius empty-repo exemption.
    if matches!(visible, VisibleRepos::Only(_)) {
        let uids: Vec<_> = result
            .seeds
            .iter()
            .chain(&result.connected)
            .filter(|node| node.uid.starts_with("sym:"))
            .map(|node| node.uid.as_str())
            .collect();
        let symbols = store.batch_lookup_symbols(&uids)?;
        let allowed: HashSet<_> = symbols
            .into_values()
            .filter(|symbol| repo_is_visible(&symbol.repo_uid, Some(visible)))
            .map(|symbol| symbol.uid)
            .collect();
        result.seeds.retain(|node| allowed.contains(&node.uid));
        result.connected.retain(|node| allowed.contains(&node.uid));
        // These diagnostics were calculated before visibility filtering and
        // cannot be attributed to the retained population. In particular,
        // hidden candidates must not leak via aggregate counts or PRF terms.
        result.seed_matches_total = None;
        result.seed_matches_total_relation = None;
        result.seeds_truncated = None;
        result.seed_resolution_limit = None;
        result.admitted_before_cap = None;
        result.expansion_terms.clear();
        result.semantic_seed_count = 0;
        result.semantic_applied = false;
        result.semantic_unavailable = None;
        result.degraded_components.clear();
    }
    let sort = |a: &crate::BrainNode, b: &crate::BrainNode| {
        b.relevance
            .total_cmp(&a.relevance)
            .then_with(|| a.uid.cmp(&b.uid))
    };
    result.seeds.sort_by(sort);
    result.connected.sort_by(sort);
    let mut seen = HashSet::new();
    result.seeds.retain(|node| seen.insert(node.uid.clone()));
    result
        .connected
        .retain(|node| seen.insert(node.uid.clone()));
    let total_nodes = seen.len();
    result.seeds.truncate(CONTEXT_NODE_LIMIT);
    result
        .connected
        .truncate(CONTEXT_NODE_LIMIT.saturating_sub(result.seeds.len()));
    let selected: Vec<_> = result
        .seeds
        .iter()
        .chain(&result.connected)
        .map(|node| node.uid.clone())
        .collect();
    let edges = store.with_read_deadline(Instant::now() + Duration::from_secs(5), || {
        store.context_edges(&selected)
    })?;
    ensure_context_generation(store, generation)?;
    let omitted_nodes = total_nodes.saturating_sub(selected.len());
    let omitted_edges = edges.total.saturating_sub(edges.edges.len());
    Ok(ContextGraph {
        edges: edges.edges,
        meta: ContextGraphMeta {
            generation,
            node_limit: CONTEXT_NODE_LIMIT,
            edge_limit: CONTEXT_EDGE_LIMIT,
            omitted_nodes,
            omitted_edges,
            edge_count_relation: if edges.total_exact { "eq" } else { "gte" },
            truncated: omitted_nodes > 0 || omitted_edges > 0 || !edges.total_exact,
            edge_scope: "returned_nodes",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

    fn node(uid: &str) -> crate::BrainNode {
        crate::BrainNode {
            uid: uid.into(),
            kind: "Symbol/Function".into(),
            title: uid.into(),
            location: "fixture.js:1".into(),
            relevance: 1.0,
            inline_body: None,
            body_complete: true,
        }
    }

    fn fixture() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        for (uid, repo) in [
            ("sym:a", "repo:allowed"),
            ("sym:b", "repo:allowed"),
            ("sym:c", "repo:hidden"),
            ("sym:d", ""),
        ] {
            store
                .insert_symbol(&Symbol {
                    uid: uid.into(),
                    name: uid.into(),
                    kind: SymbolKind::Function,
                    repo_uid: repo.into(),
                    file_path: "fixture.js".into(),
                    start_line: 1,
                    end_line: 1,
                    signature: "function fixture()".into(),
                    summary: None,
                    content_hash: "fixture".into(),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: Visibility::Public,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
        }
        for (source, target) in [("sym:a", "sym:b"), ("sym:b", "sym:c")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: source.into(),
                    target_uid: target.into(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.8,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }
        store
    }

    fn context() -> BrainContextResult {
        BrainContextResult {
            seeds: vec![node("sym:a")],
            connected: vec![node("sym:d"), node("sym:c"), node("sym:b")],
            ..Default::default()
        }
    }

    #[test]
    fn real_edges_do_not_connect_an_unrelated_node() {
        let store = fixture();
        let graph = attach_context_graph(
            &store,
            &mut context(),
            &VisibleRepos::All,
            store.graph_generation(),
        )
        .unwrap();
        assert_eq!(graph.edges.len(), 2);
        assert!(graph.edges.iter().all(|edge| edge.edge_type == "CALLS"
            && edge.source != "sym:d"
            && edge.target != "sym:d"));
        assert_eq!(graph.meta.omitted_edges, 0);
        assert_eq!(graph.meta.edge_count_relation, "eq");
    }

    #[test]
    fn restricted_scope_cannot_leak_hidden_edges_or_omission_counts() {
        let store = fixture();
        let visible = VisibleRepos::Only(HashSet::from(["repo:allowed".into()]));
        let mut context = context();
        context.seed_matches_total = Some(999);
        context.seed_matches_total_relation = Some("gte".into());
        context.seeds_truncated = Some(true);
        context.seed_resolution_limit = Some(100);
        context.admitted_before_cap = Some(999);
        context.expansion_terms = vec!["hidden-repository-term".into()];
        context.semantic_seed_count = 5;
        context.semantic_applied = true;
        context.semantic_unavailable = Some(serde_json::json!({"hidden": "detail"}));
        context.degraded_components = vec!["semantic".into()];
        let graph =
            attach_context_graph(&store, &mut context, &visible, store.graph_generation()).unwrap();
        assert_eq!(
            context
                .connected
                .iter()
                .map(|node| node.uid.as_str())
                .collect::<Vec<_>>(),
            ["sym:b"]
        );
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(
            (&*graph.edges[0].source, &*graph.edges[0].target),
            ("sym:a", "sym:b")
        );
        assert_eq!(graph.meta.omitted_nodes, 0);
        assert_eq!(graph.meta.omitted_edges, 0);
        let serialized = serde_json::to_value(&context).unwrap();
        for key in [
            "seed_matches_total",
            "seed_matches_total_relation",
            "seeds_truncated",
            "seed_resolution_limit",
            "admitted_before_cap",
            "expansion_terms",
            "semantic_seed_count",
            "semantic_unavailable",
        ] {
            assert!(
                serialized.get(key).is_none(),
                "unscoped diagnostic leaked: {key}"
            );
        }
        assert!(!context.semantic_applied);
        assert!(context.degraded_components.is_empty());
        let denied = attach_context_graph(
            &store,
            &mut context,
            &VisibleRepos::Only(HashSet::new()),
            store.graph_generation(),
        )
        .unwrap();
        assert!(denied.edges.is_empty());
        assert!(context.seeds.is_empty() && context.connected.is_empty());
    }

    #[test]
    fn changed_generation_refuses_context_instead_of_labeling_it_stable() {
        let store = fixture();
        let generation = store.graph_generation();
        store.bump_graph_generation();
        assert!(
            attach_context_graph(&store, &mut context(), &VisibleRepos::All, generation).is_err()
        );
    }

    #[test]
    fn watcher_batch_publication_does_not_refuse_context_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let generation = store.graph_generation();
        let _authority = nestweaver_store::acquire_db_write_lease(&db_path).unwrap();
        std::fs::write(
            nestweaver_store::index_publication::marker_path(&db_path),
            nestweaver_store::index_publication::format_marker_payload(
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                Some(nestweaver_store::index_publication::MARKER_REASON_WATCHER_BATCH),
            ),
        )
        .unwrap();
        ensure_context_generation(&store, generation)
            .expect("a live watcher batch must not fail HTTP context closed");
    }

    #[test]
    fn aged_out_watcher_batch_publication_refuses_context_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let generation = store.graph_generation();
        let _authority = nestweaver_store::acquire_db_write_lease(&db_path).unwrap();
        std::fs::write(
            nestweaver_store::index_publication::marker_path(&db_path),
            nestweaver_store::index_publication::format_marker_payload(
                std::process::id(),
                1,
                Some(nestweaver_store::index_publication::MARKER_REASON_WATCHER_BATCH),
            ),
        )
        .unwrap();
        assert!(
            ensure_context_generation(&store, generation).is_err(),
            "a leftover watcher-batch marker older than the debounce window must fail closed"
        );
    }

    #[test]
    fn ordinary_index_publication_still_refuses_context_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let generation = store.graph_generation();
        let _authority = nestweaver_store::acquire_db_write_lease(&db_path).unwrap();
        std::fs::write(
            nestweaver_store::index_publication::marker_path(&db_path),
            nestweaver_store::index_publication::format_marker_payload(std::process::id(), 1, None),
        )
        .unwrap();
        assert!(
            ensure_context_generation(&store, generation).is_err(),
            "a full index publication must still refuse ranked HTTP context"
        );
    }

    #[test]
    fn node_cap_is_stable_and_keeps_seeds_first() {
        let store = GraphStore::in_memory().unwrap();
        let mut result = BrainContextResult {
            seeds: vec![node("sym:seed")],
            connected: (0..510)
                .rev()
                .map(|n| node(&format!("sym:{n:04}")))
                .collect(),
            ..Default::default()
        };
        let graph = attach_context_graph(
            &store,
            &mut result,
            &VisibleRepos::All,
            store.graph_generation(),
        )
        .unwrap();
        assert_eq!(result.seeds[0].uid, "sym:seed");
        assert_eq!(result.connected.len(), 499);
        assert_eq!(result.connected[0].uid, "sym:0000");
        assert_eq!(graph.meta.omitted_nodes, 11);
        assert!(graph.meta.truncated);
    }
}
