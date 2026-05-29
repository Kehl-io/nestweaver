//! Export the LadybugDB graph store to an [`InMemoryGraph`] for MessagePack
//! serialization and WASM-side algorithm execution.

use std::collections::HashMap;

use nestweaver_algorithms::graph::{EdgeKind, InMemoryGraph, NodeMeta};
use nestweaver_store::GraphStore;

/// Build an [`InMemoryGraph`] from all symbols and code edges in the store.
///
/// Symbol nodes are loaded via `list_all_symbols`; edges are loaded via
/// `load_typed_edges` which returns `(src_uid, tgt_uid, edge_type_str,
/// confidence)` tuples. Both endpoints must be present in the symbol set for
/// an edge to be included (cross-repo orphan edges are silently dropped).
pub fn export_in_memory_graph(store: &GraphStore) -> anyhow::Result<InMemoryGraph> {
    // ── 1. Nodes ──────────────────────────────────────────────────────────────
    let symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut uids: Vec<String> = Vec::with_capacity(symbols.len());
    let mut nodes: Vec<NodeMeta> = Vec::with_capacity(symbols.len());
    let mut uid_to_idx: HashMap<String, u32> = HashMap::with_capacity(symbols.len());

    for sym in &symbols {
        let idx = uids.len() as u32;
        uid_to_idx.insert(sym.uid.clone(), idx);
        uids.push(sym.uid.clone());
        nodes.push(NodeMeta {
            name: sym.name.clone(),
            kind: sym.kind.to_string(),
            file_path: Some(sym.file_path.clone()),
            pagerank_score: sym.pagerank_score,
            is_entry_point: sym.is_entry_point,
        });
    }

    // ── 2. Edges ──────────────────────────────────────────────────────────────
    // load_typed_edges returns (src_uid, tgt_uid, edge_type_str, confidence).
    // Edge type strings match the DB table names:
    //   CALLS, IMPORTS, EXTENDS_SYM, IMPLEMENTS_SYM, USES, ACCESSES,
    //   MEMBER_OF, INCLUDES_SYM
    let typed_edges = store
        .load_typed_edges()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut edges: Vec<(u32, u32, f32, EdgeKind)> = Vec::with_capacity(typed_edges.len());

    for (src_uid, tgt_uid, edge_type, confidence) in &typed_edges {
        let (Some(&si), Some(&ti)) = (uid_to_idx.get(src_uid), uid_to_idx.get(tgt_uid)) else {
            continue;
        };
        let kind = parse_edge_kind(edge_type);
        edges.push((si, ti, *confidence as f32, kind));
    }

    // ── 3. Generation counter ─────────────────────────────────────────────────
    let generation = store.graph_generation();

    Ok(InMemoryGraph {
        uids,
        nodes,
        edges,
        generation,
    })
}

fn parse_edge_kind(s: &str) -> EdgeKind {
    match s {
        "CALLS" => EdgeKind::Calls,
        "IMPORTS" => EdgeKind::Imports,
        "EXTENDS_SYM" => EdgeKind::Extends,
        "IMPLEMENTS_SYM" => EdgeKind::Implements,
        "USES" => EdgeKind::Uses,
        "ACCESSES" => EdgeKind::Accesses,
        "MEMBER_OF" => EdgeKind::MemberOf,
        "INCLUDES_SYM" => EdgeKind::Includes,
        _ => EdgeKind::Other,
    }
}
