use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Edge type classification for weight computation.
///
/// Mirrors the edge types in `nestweaver-schema` but is kept independent so
/// that this crate has zero internal dependencies and can be compiled to WASM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Imports,
    Extends,
    Implements,
    Uses,
    Accesses,
    MemberOf,
    Includes,
    ProjectIncludesNote,
    ProjectIncludesSymbol,
    WikilinkToNote,
    WikilinkToHeading,
    Other,
}

/// Metadata for a single node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMeta {
    pub name: String,
    pub kind: String,
    pub file_path: Option<String>,
    pub pagerank_score: Option<f64>,
    pub is_entry_point: bool,
}

/// A lightweight in-memory graph for WASM and offline algorithm execution.
///
/// Edges are stored as `(source_idx, target_idx, confidence, kind)` where the
/// indices refer to positions in `uids` / `nodes`. This flat representation is
/// efficient for serialization (msgpack) and for building adjacency structures
/// on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InMemoryGraph {
    pub uids: Vec<String>,
    pub nodes: Vec<NodeMeta>,
    /// `(source_idx, target_idx, confidence, kind)`.
    pub edges: Vec<(u32, u32, f32, EdgeKind)>,
    /// Monotonically increasing generation counter — bumped on every graph
    /// mutation so consumers can detect staleness.
    pub generation: u64,
}

impl InMemoryGraph {
    /// Build the adjacency structures needed by PPR from the edge list.
    ///
    /// Both forward and reverse edges are included (reverse at 30% weight)
    /// so that PPR propagates relevance through the full neighbourhood,
    /// matching the existing `load_ppr_graph` behaviour in `nestweaver-store`.
    pub fn build_adjacency(&self, edge_weights: &EdgeWeightConfig) -> AdjacencyData {
        let n = self.uids.len();
        let uid_to_idx: HashMap<String, usize> = self
            .uids
            .iter()
            .enumerate()
            .map(|(i, uid)| (uid.clone(), i))
            .collect();

        let mut out_weight: Vec<f64> = vec![0.0; n];
        let mut incoming: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];

        for &(src, tgt, conf, kind) in &self.edges {
            let si = src as usize;
            let ti = tgt as usize;
            if si >= n || ti >= n || conf <= 0.0 {
                continue;
            }

            let base = edge_weights.base_weight(kind);
            let multiplier = edge_weights.intent_multiplier(kind);
            let w = conf as f64 * base * multiplier;

            // Forward edge
            out_weight[si] += w;
            incoming[ti].push((si, w));
            // Reverse edge at 30%
            let rw = w * 0.3;
            out_weight[ti] += rw;
            incoming[si].push((ti, rw));
        }

        AdjacencyData {
            uid_to_idx,
            incoming,
            out_weight,
        }
    }
}

/// Pre-computed adjacency data for PPR iteration.
pub struct AdjacencyData {
    pub uid_to_idx: HashMap<String, usize>,
    /// For each node `v`, the list of `(u, weight)` pairs where `u` has an
    /// edge pointing to `v`.
    pub incoming: Vec<Vec<(usize, f64)>>,
    /// Sum of all outgoing edge weights per node.
    pub out_weight: Vec<f64>,
}

/// Configuration for edge type weighting.
///
/// Intent-specific multipliers are applied on top of the base weights so
/// that, for example, `AnalyzeImpact` can boost CALLS edges without
/// changing the base coupling model.
pub struct EdgeWeightConfig {
    pub calls_multiplier: f64,
    pub project_includes_multiplier: f64,
}

impl EdgeWeightConfig {
    pub fn default_config() -> Self {
        Self {
            calls_multiplier: 1.0,
            project_includes_multiplier: 1.0,
        }
    }

    /// Base coupling weight for each edge kind.
    ///
    /// These model coupling strength:
    /// - CALLS          1.0 — direct invocation is strongest coupling
    /// - EXTENDS/IMPL   0.9 — inheritance is near-call coupling
    /// - IMPORTS         0.7 — dependency without call detail
    /// - USES            0.5 — type reference is real but weaker
    /// - ACCESSES        0.4 — field access is medium coupling
    /// - MEMBER_OF etc.  0.2 — structural containment
    pub fn base_weight(&self, kind: EdgeKind) -> f64 {
        match kind {
            EdgeKind::Calls => 1.0,
            EdgeKind::Extends | EdgeKind::Implements => 0.9,
            EdgeKind::Imports => 0.7,
            EdgeKind::Uses => 0.5,
            EdgeKind::Accesses => 0.4,
            EdgeKind::MemberOf | EdgeKind::Includes => 0.2,
            EdgeKind::ProjectIncludesNote | EdgeKind::ProjectIncludesSymbol => 1.0,
            _ => 1.0,
        }
    }

    /// Intent-specific multiplier layered on top of the base weight.
    pub fn intent_multiplier(&self, kind: EdgeKind) -> f64 {
        match kind {
            EdgeKind::Calls => self.calls_multiplier,
            EdgeKind::ProjectIncludesNote | EdgeKind::ProjectIncludesSymbol => {
                self.project_includes_multiplier
            }
            _ => 1.0,
        }
    }
}
