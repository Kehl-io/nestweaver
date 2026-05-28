use std::collections::{HashMap, HashSet};
use std::fmt;

use lbug::Value;
use nestweaver_schema::{EdgeType, Symbol};
use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::StoreError;
use crate::read::{SYMBOL_COLUMNS, row_to_symbol};

/// Describes the intent behind a PPR query, allowing the algorithm to
/// adapt its damping factor (alpha) and edge weights to produce more
/// relevant results for different use cases.
///
/// The damping factor (alpha) controls the probability that the random
/// walk teleports back to a seed node at each step. Higher alpha means
/// more local focus around the seeds; lower alpha means broader
/// exploration of the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QueryIntent {
    /// Navigating to a specific definition. High alpha (0.5) keeps
    /// results tightly focused around the seed symbol.
    FindDefinition,
    /// Understanding the overall architecture. Low alpha (0.15) lets
    /// the walk explore distant graph regions to surface the structural
    /// skeleton.
    UnderstandArchitecture,
    /// Analyzing blast radius / impact of a change. Alpha 0.3, with
    /// CALLS edges weighted 2x to emphasize call-chain propagation.
    AnalyzeImpact,
    /// General-purpose context retrieval. Alpha 0.25 balances local
    /// relevance with some exploration. This is the default behaviour
    /// when no intent is specified.
    GeneralContext,
    /// Project-scoped context retrieval. Alpha 0.3 (moderate exploration)
    /// with 5x weight on PROJECT_INCLUDES_NOTE and PROJECT_INCLUDES_SYMBOL
    /// edges so that a project's declared content dominates the ranking
    /// over high-in-degree generic notes.
    ProjectContext,
}

impl QueryIntent {
    /// Return the damping factor for this intent.
    ///
    /// The damping factor `d` is used in the PPR formula:
    ///   score(v) = (1 - d) * personalization(v) + d * Σ(neighbours)
    ///
    /// Higher `d` means more propagation through the graph (wider
    /// exploration). The "alpha" described in the enum doc comments
    /// refers to `1 - d` (the teleport/restart probability).
    pub fn damping(&self) -> f64 {
        match self {
            // alpha=0.5 → d=0.5 (high restart probability, local focus)
            QueryIntent::FindDefinition => 0.5,
            // alpha=0.15 → d=0.85 (standard, broad exploration)
            QueryIntent::UnderstandArchitecture => 0.85,
            // alpha=0.3 → d=0.7 (moderate exploration, impact chains)
            QueryIntent::AnalyzeImpact => 0.7,
            // alpha=0.25 → d=0.75 (balanced default)
            QueryIntent::GeneralContext => 0.75,
            // alpha=0.3 → d=0.7 (moderate exploration, project scope)
            QueryIntent::ProjectContext => 0.7,
        }
    }

    /// Return a multiplier for CALLS edges under this intent.
    /// Non-CALLS edges use a multiplier of 1.0.
    pub fn calls_weight_multiplier(&self) -> f64 {
        match self {
            QueryIntent::AnalyzeImpact => 2.0,
            _ => 1.0,
        }
    }

    /// Return a multiplier for PROJECT_INCLUDES_NOTE and
    /// PROJECT_INCLUDES_SYMBOL edges under this intent.
    /// Non-project-includes edges use a multiplier of 1.0.
    pub fn project_includes_weight_multiplier(&self) -> f64 {
        match self {
            QueryIntent::ProjectContext => 5.0,
            _ => 1.0,
        }
    }
}

impl fmt::Display for QueryIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryIntent::FindDefinition => write!(f, "find-definition"),
            QueryIntent::UnderstandArchitecture => write!(f, "understand-architecture"),
            QueryIntent::AnalyzeImpact => write!(f, "analyze-impact"),
            QueryIntent::GeneralContext => write!(f, "general-context"),
            QueryIntent::ProjectContext => write!(f, "project-context"),
        }
    }
}

impl std::str::FromStr for QueryIntent {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "find-definition" | "definition" | "find" => Ok(QueryIntent::FindDefinition),
            "understand-architecture" | "architecture" | "arch" => {
                Ok(QueryIntent::UnderstandArchitecture)
            }
            "analyze-impact" | "impact" | "blast-radius" => Ok(QueryIntent::AnalyzeImpact),
            "general-context" | "general" | "context" => Ok(QueryIntent::GeneralContext),
            "project-context" | "project" => Ok(QueryIntent::ProjectContext),
            other => Err(format!(
                "unknown intent '{}': expected one of find-definition, \
                 understand-architecture, analyze-impact, general-context, \
                 project-context",
                other
            )),
        }
    }
}

/// Auto-detect the most likely `QueryIntent` from the resolved seed UIDs
/// and the graph store.
///
/// Heuristics:
/// - Single seed → `FindDefinition` (the user is zooming in on one symbol)
/// - Multiple seeds from different files → `UnderstandArchitecture`
/// - Any seed is an entry point → `AnalyzeImpact`
/// - Otherwise → `GeneralContext`
pub fn detect_intent(store: &GraphStore, seed_uids: &[String]) -> QueryIntent {
    if seed_uids.is_empty() {
        return QueryIntent::GeneralContext;
    }

    // Check for entry points first — this signals impact analysis.
    let mut files_seen: HashSet<String> = HashSet::new();
    let mut any_entry_point = false;

    for uid in seed_uids {
        if let Ok(sym) = store.lookup_symbol(uid) {
            if sym.is_entry_point {
                any_entry_point = true;
            }
            files_seen.insert(sym.file_path.clone());
        }
        // Non-symbol UIDs (notes, headings, etc.) are ignored for intent
        // detection — they don't have file_path or entry_point semantics.
    }

    if any_entry_point {
        return QueryIntent::AnalyzeImpact;
    }

    if seed_uids.len() == 1 {
        return QueryIntent::FindDefinition;
    }

    if files_seen.len() > 1 {
        return QueryIntent::UnderstandArchitecture;
    }

    QueryIntent::GeneralContext
}

/// Internal adjacency data returned by `load_ppr_graph`.
///
/// Fields: (uids, uid_to_idx, incoming adjacency weighted by edge confidence,
///          total outgoing weight per node)
///
/// Each entry in `incoming[v]` is `(u, weight)` — the source node index and
/// the normalised transition weight for the edge u→v. `out_weight[u]` is the
/// sum of all outgoing weights from node `u` (pre-normalisation denominator).
type PprGraph = (
    Vec<String>,
    HashMap<String, usize>,
    Vec<Vec<(usize, f64)>>,
    Vec<f64>,
);

/// Describes which slice of the graph PageRank / PPR runs over.
///
/// The algorithm itself is node-type-agnostic — `GraphScope` is what turns
/// it into "rank the code graph", "rank the notes graph", or "rank the
/// unified graph". Each scope is a set of Cypher queries:
///
/// - `node_queries` — each must `RETURN` a single column of UIDs. Results
///   are unioned to produce the full set of nodes in scope.
/// - `edge_queries` — each must `RETURN` three columns `(source_uid,
///   target_uid, confidence)`. Confidence is used to weight PPR transitions.
///   Edges with a missing/null confidence column default to weight 1.0.
///   Edges whose confidence rounds to 0.0 are skipped (unresolved imports
///   should not influence ranking).
///
/// Preset constructors `code_only`, `notes_only`, and `unified` cover the
/// common cases. Custom scopes can be constructed directly for one-off
/// queries (e.g. "PPR only within one project").
#[derive(Debug, Clone)]
pub struct GraphScope {
    pub node_queries: Vec<String>,
    pub edge_queries: Vec<String>,
}

impl GraphScope {
    /// The original PPR scope: Symbol nodes + the eight code edge types
    /// (CALLS, IMPORTS, EXTENDS_SYM, IMPLEMENTS_SYM, MEMBER_OF, INCLUDES_SYM,
    /// USES, ACCESSES). Edge confidence is returned as the third column to
    /// weight PPR transitions.
    pub fn code_only() -> Self {
        let code_edge_types = [
            EdgeType::Calls,
            EdgeType::Imports,
            EdgeType::Extends,
            EdgeType::Implements,
            EdgeType::MemberOf,
            EdgeType::Includes,
            EdgeType::Uses,
            EdgeType::Accesses,
        ];
        Self {
            node_queries: vec!["MATCH (s:Symbol) RETURN s.uid".to_string()],
            edge_queries: code_edge_types
                .iter()
                .map(|et| {
                    format!(
                        "MATCH (a:Symbol)-[r:{}]->(b:Symbol) RETURN a.uid, b.uid, r.confidence",
                        et.rel_table_name()
                    )
                })
                .collect(),
        }
    }

    /// Notes-domain scope: Note + Heading + Section + Tag nodes; containment
    /// edges (NOTE_HAS_HEADING, NOTE_HAS_SECTION, HEADING_HAS_SECTION,
    /// HEADING_PARENT), cross-reference edges (WIKILINK_TO_NOTE,
    /// WIKILINK_TO_HEADING), and tag edges (NOTE_TAGGED_WITH,
    /// SECTION_TAGGED_WITH). Wikilinks carry a confidence score; structural
    /// edges default to 1.0 (the null column is filled in by load_ppr_graph).
    pub fn notes_only() -> Self {
        Self {
            node_queries: vec![
                "MATCH (n:Note) RETURN n.uid".to_string(),
                "MATCH (h:Heading) RETURN h.uid".to_string(),
                "MATCH (s:Section) RETURN s.uid".to_string(),
                "MATCH (t:Tag) RETURN t.uid".to_string(),
            ],
            edge_queries: vec![
                // Structural containment edges: no confidence property → defaults to 1.0.
                "MATCH (a:Note)-[:NOTE_HAS_HEADING]->(b:Heading) RETURN a.uid, b.uid".to_string(),
                "MATCH (a:Note)-[:NOTE_HAS_SECTION]->(b:Section) RETURN a.uid, b.uid".to_string(),
                "MATCH (a:Heading)-[:HEADING_HAS_SECTION]->(b:Section) RETURN a.uid, b.uid"
                    .to_string(),
                "MATCH (a:Heading)-[:HEADING_PARENT]->(b:Heading) RETURN a.uid, b.uid".to_string(),
                // Wikilinks carry confidence — query the property explicitly.
                "MATCH (a:Section)-[r:WIKILINK_TO_NOTE]->(b:Note) RETURN a.uid, b.uid, r.confidence"
                    .to_string(),
                "MATCH (a:Section)-[r:WIKILINK_TO_HEADING]->(b:Heading) RETURN a.uid, b.uid, r.confidence"
                    .to_string(),
                // Tag edges: no confidence property → defaults to 1.0.
                "MATCH (a:Note)-[:NOTE_TAGGED_WITH]->(b:Tag) RETURN a.uid, b.uid".to_string(),
                "MATCH (a:Section)-[:SECTION_TAGGED_WITH]->(b:Tag) RETURN a.uid, b.uid".to_string(),
            ],
        }
    }

    /// Unified scope spanning code + notes + the cross-domain bridges
    /// that PPR traverses to rank a Symbol and a Note on the same axis.
    /// This is the single graph the brain answers queries from.
    pub fn unified() -> Self {
        let mut scope = Self::code_only();
        let notes = Self::notes_only();
        scope.node_queries.extend(notes.node_queries);
        scope.edge_queries.extend(notes.edge_queries);
        // Cross-domain bridges — the architectural keystone.
        scope.edge_queries.push(
            "MATCH (a:Note)-[r:REFERENCES_CODE_NOTE_TO_SYMBOL]->(b:Symbol) RETURN a.uid, b.uid, r.confidence"
                .to_string(),
        );
        scope.edge_queries.push(
            "MATCH (a:Section)-[r:REFERENCES_CODE_SECTION_TO_SYMBOL]->(b:Symbol) RETURN a.uid, b.uid, r.confidence"
                .to_string(),
        );
        // Project nodes and edges — allows PPR to traverse project membership.
        scope
            .node_queries
            .push("MATCH (p:Project) RETURN p.uid".to_string());
        scope.edge_queries.push(
            "MATCH (p:Project)-[r:PROJECT_INCLUDES_NOTE]->(n:Note) RETURN p.uid, n.uid, r.confidence"
                .to_string(),
        );
        scope.edge_queries.push(
            "MATCH (p:Project)-[r:PROJECT_INCLUDES_SYMBOL]->(s:Symbol) RETURN p.uid, s.uid, r.confidence"
                .to_string(),
        );
        scope.edge_queries.push(
            "MATCH (p:Project)-[r:PROJECT_HAS_COMPONENT]->(q:Project) RETURN p.uid, q.uid, r.confidence"
                .to_string(),
        );
        scope
    }
}

impl GraphStore {
    /// Compute PageRank over the nodes and edges in `scope`.
    ///
    /// - `damping`: damping factor (typically 0.85)
    /// - `iterations`: number of iterations to run
    /// - `scope`: which slice of the graph to rank
    ///
    /// After completion the in-memory `pagerank_cache` is replaced with
    /// scores keyed by node UID. Use `GraphScope::code_only()` to preserve
    /// the original Symbol-only behaviour; `GraphScope::unified()` ranks
    /// code + notes on the same axis (this is what the brain queries use).
    pub fn compute_pagerank(
        &self,
        damping: f64,
        iterations: u32,
        scope: &GraphScope,
    ) -> Result<(), StoreError> {
        let (uids, _uid_to_idx, incoming, out_weight) = self.load_ppr_graph(scope, None)?;
        let n = uids.len();
        if n == 0 {
            return Ok(());
        }

        let init = 1.0f64 / n as f64;
        let mut scores: Vec<f64> = vec![init; n];
        let teleport = (1.0 - damping) / n as f64;

        for _ in 0..iterations {
            let mut new_scores: Vec<f64> = vec![teleport; n];

            let dangling_sum: f64 = scores
                .iter()
                .enumerate()
                .filter(|&(i, _)| out_weight[i] == 0.0)
                .map(|(_, &s)| s)
                .sum::<f64>();
            let dangling_contrib = damping * dangling_sum / n as f64;

            for v in 0..n {
                new_scores[v] += dangling_contrib;
                for &(u, w) in &incoming[v] {
                    if out_weight[u] > 0.0 {
                        new_scores[v] += damping * scores[u] * w / out_weight[u];
                    }
                }
            }

            let delta: f64 = new_scores
                .iter()
                .zip(scores.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0_f64, f64::max);

            scores = new_scores;

            if delta < 1e-6 {
                break;
            }
        }

        // 5. Store scores in the in-memory cache (no DB write-back needed).
        let score_map: HashMap<String, f64> = uids
            .into_iter()
            .enumerate()
            .map(|(i, uid)| (uid, scores[i]))
            .collect();
        *self
            .pagerank_cache
            .lock()
            .map_err(|e| StoreError::Query(format!("lock: {e}")))? = Some(score_map);

        // Bump the generation so cache-holders know to refresh.
        self.bump_pagerank_generation();

        Ok(())
    }

    /// Load all node UIDs and directed edges in `scope` into adjacency
    /// structures.
    ///
    /// Returns `(uids, uid_to_idx, incoming, out_weight)` where:
    /// - `uids`       — ordered list of all node UIDs in scope
    /// - `uid_to_idx` — maps uid → index
    /// - `incoming`   — for each node v, the list of `(u, weight)` pairs where
    ///   u has an edge u→v with the given weight
    /// - `out_weight` — sum of all outgoing edge weights per node
    ///
    /// Edge weight is taken from the optional third column (`r.confidence`).
    /// Missing / null confidence defaults to 1.0. Edges with confidence ≤ 0.0
    /// are filtered out (unresolved imports should not influence ranking).
    ///
    /// When `intent` is provided, edge weights are adjusted: for
    /// `AnalyzeImpact`, CALLS edges receive a 2x multiplier. The intent
    /// is identified from the edge query string (queries containing
    /// `:CALLS]` are treated as CALLS edges).
    ///
    /// Both forward and reverse directions are included so that PPR propagates
    /// relevance through the full neighbourhood.
    fn load_ppr_graph(
        &self,
        scope: &GraphScope,
        intent: Option<QueryIntent>,
    ) -> Result<PprGraph, StoreError> {
        let conn = self.conn()?;

        // 1. Load all node UIDs in scope (deduplicated across queries).
        let mut uid_set: HashSet<String> = HashSet::new();
        let mut uids: Vec<String> = Vec::new();
        for q in &scope.node_queries {
            let rows = conn
                .query(q)
                .map_err(|e| StoreError::Query(e.to_string()))?;
            for row in rows {
                if let Some(Value::String(s)) = row.first()
                    && uid_set.insert(s.clone())
                {
                    uids.push(s.clone());
                }
            }
        }
        drop(uid_set);

        let n = uids.len();
        let uid_to_idx: HashMap<String, usize> = uids
            .iter()
            .enumerate()
            .map(|(i, uid)| (uid.clone(), i))
            .collect();

        // 2. Load directed edges from each scope query. Edges carry a confidence
        //    weight from the optional third column; missing/null → 1.0.
        //    Edges with confidence ≤ 0.0 are skipped (unresolved).
        let calls_multiplier = intent.map_or(1.0, |i| i.calls_weight_multiplier());
        let project_includes_multiplier =
            intent.map_or(1.0, |i| i.project_includes_weight_multiplier());
        let mut forward_edges: Vec<(usize, usize, f64)> = Vec::new();
        for q in &scope.edge_queries {
            // Detect edge type from the query string to apply type-aware
            // base weights and intent multipliers.
            //
            // Base weights model coupling strength:
            //   CALLS          1.0  — direct invocation is strongest coupling
            //   EXTENDS/IMPL   0.9  — inheritance is near-call coupling
            //   IMPORTS         0.7  — dependency without call detail
            //   USES            0.5  — type reference is real but weaker
            //   ACCESSES        0.4  — field access is medium coupling
            //   MEMBER_OF etc.  0.2  — structural containment
            //
            // Intent multipliers are layered on top of the base weight.
            let base_weight = if q.contains(":CALLS]") {
                1.0
            } else if q.contains(":EXTENDS_SYM]") || q.contains(":IMPLEMENTS_SYM]") {
                0.9
            } else if q.contains(":IMPORTS]") {
                0.7
            } else if q.contains(":USES]") {
                0.5
            } else if q.contains(":ACCESSES]") {
                0.4
            } else if q.contains(":MEMBER_OF]") || q.contains(":INCLUDES_SYM]") {
                0.2
            } else {
                1.0
            };
            let is_calls_query = q.contains(":CALLS]");
            let is_project_includes_query =
                q.contains(":PROJECT_INCLUDES_NOTE") || q.contains(":PROJECT_INCLUDES_SYMBOL");
            let intent_multiplier = if is_calls_query {
                calls_multiplier
            } else if is_project_includes_query {
                project_includes_multiplier
            } else {
                1.0
            };
            let edge_multiplier = base_weight * intent_multiplier;

            let rows = match conn.query(q) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!(
                        "load_ppr_graph: edge query skipped (table may not exist yet): {e}"
                    );
                    continue;
                }
            };
            for row in rows {
                let src = match row.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => continue,
                };
                let tgt = match row.get(1) {
                    Some(Value::String(s)) => s.clone(),
                    _ => continue,
                };
                // Third column is optional confidence. Null / missing → 1.0.
                let conf: f64 = match row.get(2) {
                    Some(Value::Double(c)) => *c,
                    Some(Value::Float(c)) => *c as f64,
                    Some(Value::Int64(c)) => *c as f64,
                    _ => 1.0,
                };
                // Skip zero-confidence (unresolved) edges.
                if conf <= 0.0 {
                    continue;
                }
                if let (Some(&si), Some(&ti)) = (uid_to_idx.get(&src), uid_to_idx.get(&tgt)) {
                    forward_edges.push((si, ti, conf * edge_multiplier));
                }
            }
        }

        // Combine forward + reverse for neighbourhood propagation.
        // Reverse edges use 30% of the forward weight so PPR can traverse
        // backwards (discovery) while preserving the directional signal.
        let mut all_edges: Vec<(usize, usize, f64)> = Vec::with_capacity(forward_edges.len() * 2);
        for &(u, v, w) in &forward_edges {
            all_edges.push((u, v, w));
            all_edges.push((v, u, w * 0.3)); // reverse edge at 30% weight
        }

        // Compute total outgoing weight and weighted incoming adjacency.
        let mut out_weight: Vec<f64> = vec![0.0f64; n];
        let mut incoming: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        for &(u, v, w) in &all_edges {
            out_weight[u] += w;
            incoming[v].push((u, w));
        }

        Ok((uids, uid_to_idx, incoming, out_weight))
    }

    /// Run Personalized PageRank seeded from `seed_uids`.
    ///
    /// The personalization vector assigns `1/|seeds|` to each seed node and `0`
    /// to all others.  Convergence is declared when the maximum absolute change
    /// across all nodes falls below `1e-6`.
    ///
    /// When `intent` is `Some`, the damping factor is taken from the intent
    /// (overriding `damping`) and CALLS edges may receive extra weight.
    /// When `intent` is `None`, the caller-provided `damping` is used and
    /// all edges keep their default weight — preserving backward compatibility.
    ///
    /// Returns `(uid, score)` pairs sorted descending, filtered to `score > 1e-4`.
    /// Seed nodes are always included regardless of score.
    pub fn personalized_pagerank(
        &self,
        seed_uids: &[String],
        damping: f64,
        max_iterations: u32,
        scope: &GraphScope,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        self.personalized_pagerank_with_intent(seed_uids, damping, max_iterations, scope, None)
    }

    /// Like [`personalized_pagerank`] but accepts an optional [`QueryIntent`]
    /// that dynamically adjusts the damping factor and edge weights.
    pub fn personalized_pagerank_with_intent(
        &self,
        seed_uids: &[String],
        damping: f64,
        max_iterations: u32,
        scope: &GraphScope,
        intent: Option<QueryIntent>,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        let effective_damping = intent.map_or(damping, |i| i.damping());
        let (uids, uid_to_idx, incoming, out_weight) = self.load_ppr_graph(scope, intent)?;

        let n = uids.len();
        if n == 0 {
            return Ok(vec![]);
        }

        // Build seed index set for O(1) lookup.
        let seed_set: std::collections::HashSet<usize> = seed_uids
            .iter()
            .filter_map(|uid| uid_to_idx.get(uid).copied())
            .collect();

        let seed_count = seed_set.len();
        if seed_count == 0 {
            // No seeds present in the graph — return empty.
            return Ok(vec![]);
        }

        let personalization_val = 1.0 / seed_count as f64;

        // Personalization vector: 1/|seeds| for seeds, 0 otherwise.
        let personalization: Vec<f64> = (0..n)
            .map(|i| {
                if seed_set.contains(&i) {
                    personalization_val
                } else {
                    0.0
                }
            })
            .collect();

        // Initialize scores to the personalization vector.
        let mut scores: Vec<f64> = personalization.clone();

        for _ in 0..max_iterations {
            let mut new_scores: Vec<f64> = vec![0.0; n];

            // Dangling-node handling: redistribute mass from nodes with no
            // outgoing edges through the personalization vector.
            let dangling_sum: f64 = scores
                .iter()
                .enumerate()
                .filter(|&(i, _)| out_weight[i] == 0.0)
                .map(|(_, &s)| s)
                .sum::<f64>();

            for v in 0..n {
                new_scores[v] = (1.0 - effective_damping) * personalization[v]
                    + effective_damping * dangling_sum * personalization[v];

                for &(u, w) in &incoming[v] {
                    if out_weight[u] > 0.0 {
                        new_scores[v] += effective_damping * scores[u] * w / out_weight[u];
                    }
                }
            }

            // Check convergence.
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

        let min_score = 1e-4;

        // Collect results: always include seeds, filter others by min_score.
        let mut results: Vec<(String, f64)> = uids
            .iter()
            .enumerate()
            .filter(|&(i, _)| seed_set.contains(&i) || scores[i] > min_score)
            .map(|(i, uid)| (uid.clone(), scores[i]))
            .collect();

        // Sort descending by score.
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(results)
    }

    /// Return all Symbol nodes that have a pagerank_score set, ordered descending by score.
    ///
    /// Scores are read from the in-memory cache populated by `compute_pagerank`.
    /// If `limit` is `None`, all symbols are returned.
    pub fn symbols_by_pagerank(&self, limit: Option<usize>) -> Result<Vec<Symbol>, StoreError> {
        let cache = self
            .pagerank_cache
            .lock()
            .map_err(|e| StoreError::Query(format!("lock: {e}")))?;
        let scores = match cache.as_ref() {
            Some(s) => s,
            None => return Ok(Vec::new()),
        };

        // Get all symbols, attach scores from cache, sort descending.
        let conn = self.conn()?;
        let q = format!("MATCH (s:Symbol) RETURN {}", SYMBOL_COLUMNS);
        let result = conn
            .query(&q)
            .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut symbols: Vec<Symbol> = result
            .filter_map(|row| row_to_symbol(&row).ok())
            .filter_map(|mut sym| {
                scores.get(&sym.uid).copied().map(|score| {
                    sym.pagerank_score = Some(score);
                    sym
                })
            })
            .collect();

        symbols.sort_by(|a, b| {
            b.pagerank_score
                .unwrap_or(0.0)
                .partial_cmp(&a.pagerank_score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(lim) = limit {
            symbols.truncate(lim);
        }

        Ok(symbols)
    }

    /// Persist the in-memory PageRank cache to a JSON sidecar file at `path`.
    ///
    /// If the cache is empty (PageRank has not been computed yet), this is a no-op.
    pub fn save_pagerank_cache(&self, path: &std::path::Path) -> Result<(), StoreError> {
        let cache = self
            .pagerank_cache
            .lock()
            .map_err(|e| StoreError::Query(format!("lock: {e}")))?;
        if let Some(scores) = cache.as_ref() {
            let json = serde_json::to_string(scores)
                .map_err(|e| StoreError::Query(format!("serialize: {e}")))?;
            std::fs::write(path, json).map_err(|e| StoreError::Query(format!("write: {e}")))?;
        }
        Ok(())
    }

    /// Load the PageRank cache from a JSON sidecar file at `path`.
    ///
    /// If the file does not exist, this is a no-op.
    pub fn load_pagerank_cache(&self, path: &std::path::Path) -> Result<(), StoreError> {
        if path.exists() {
            let json = std::fs::read_to_string(path)
                .map_err(|e| StoreError::Query(format!("read: {e}")))?;
            let scores: HashMap<String, f64> = serde_json::from_str(&json)
                .map_err(|e| StoreError::Query(format!("deserialize: {e}")))?;
            *self
                .pagerank_cache
                .lock()
                .map_err(|e| StoreError::Query(format!("lock: {e}")))? = Some(scores);
        }
        Ok(())
    }

    /// Return a clone of the in-memory PageRank scores map.
    ///
    /// Returns an empty map if PageRank has not been computed yet or the
    /// cache is not loaded. Used by downstream crates (engine) that need
    /// per-UID score lookups without loading full Symbol objects.
    pub fn pagerank_scores(&self) -> HashMap<String, f64> {
        self.pagerank_cache
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_default()
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

    use super::GraphScope;
    use crate::db::GraphStore;

    fn make_symbol(uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo-1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
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
        }
    }

    fn make_calls_edge(src: &str, tgt: &str) -> ResolvedEdge {
        ResolvedEdge {
            source_uid: src.to_string(),
            target_uid: tgt.to_string(),
            edge_type: EdgeType::Calls,
            confidence: 1.0,
            link_type: None,
        }
    }

    fn test_store() -> GraphStore {
        GraphStore::in_memory().unwrap()
    }

    #[test]
    fn pagerank_assigns_nonzero_scores() {
        // Graph: A->B, A->C, B->C  (C has most incoming, should rank highest)
        let store = test_store();
        store.insert_symbol(&make_symbol("A", "fn_a")).unwrap();
        store.insert_symbol(&make_symbol("B", "fn_b")).unwrap();
        store.insert_symbol(&make_symbol("C", "fn_c")).unwrap();

        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("A", "C")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();

        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();

        // Scores are stored in the cache; read them via symbols_by_pagerank.
        let ranked = store.symbols_by_pagerank(None).unwrap();
        let c = ranked.iter().find(|s| s.uid == "C").unwrap();
        let score = c.pagerank_score.unwrap_or(0.0);
        assert!(
            score > 0.0,
            "C should have a nonzero pagerank score, got {score}"
        );
    }

    #[test]
    fn highly_called_symbol_ranks_higher() {
        // A->C, B->C, D->C — C has three incoming, should rank highest.
        let store = test_store();
        for uid in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "C")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("D", "C")).unwrap();

        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();

        // Scores are stored in the cache; read them via symbols_by_pagerank.
        let ranked = store.symbols_by_pagerank(None).unwrap();
        let score_of = |uid: &str| -> f64 {
            ranked
                .iter()
                .find(|s| s.uid == uid)
                .and_then(|s| s.pagerank_score)
                .unwrap_or(0.0)
        };
        let c_score = score_of("C");
        let a_score = score_of("A");
        let b_score = score_of("B");
        let d_score = score_of("D");

        assert!(
            c_score > a_score && c_score > b_score && c_score > d_score,
            "C ({c_score:.4}) should rank higher than A ({a_score:.4}), B ({b_score:.4}), D ({d_score:.4})"
        );
    }

    #[test]
    fn personalized_pagerank_seeds_rank_highest() {
        // Graph: A->B->C, D->C
        // Seed from A — A should have highest score, B next, C next.
        // D should have a low score because it is not in A's neighbourhood.
        let store = test_store();
        for uid in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("D", "C")).unwrap();

        let results = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::code_only())
            .unwrap();

        let score_of = |uid: &str| -> f64 {
            results
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0)
        };

        let a_score = score_of("A");
        let d_score = score_of("D");

        assert!(
            a_score > d_score,
            "seed A ({a_score:.6}) should rank higher than non-seed D ({d_score:.6})"
        );
        assert!(a_score > 0.0, "A should have a nonzero PPR score");
    }

    #[test]
    fn personalized_pagerank_multiple_seeds() {
        // Graph: A->B->C, D->C
        // Seed from both A and D — both should rank high, C should rank high
        // because it is reachable from both seeds' neighbourhoods.
        let store = test_store();
        for uid in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("D", "C")).unwrap();

        let results = store
            .personalized_pagerank(
                &["A".to_string(), "D".to_string()],
                0.85,
                20,
                &GraphScope::code_only(),
            )
            .unwrap();

        let score_of = |uid: &str| -> f64 {
            results
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0)
        };

        let a_score = score_of("A");
        let d_score = score_of("D");
        let c_score = score_of("C");
        let b_score = score_of("B");

        // Both seeds must appear with nonzero scores.
        assert!(a_score > 0.0, "seed A should have nonzero PPR score");
        assert!(d_score > 0.0, "seed D should have nonzero PPR score");
        // C is connected to both seeds' neighbourhoods and should outrank isolated nodes.
        assert!(
            c_score > 0.0 || b_score > 0.0,
            "connected nodes B/C should appear in results"
        );
    }

    #[test]
    fn unified_scope_includes_notes_and_symbols() {
        use nestweaver_schema::{Note, NoteKind};

        let store = test_store();
        // Symbols: A, B
        store.insert_symbol(&make_symbol("A", "fn_a")).unwrap();
        store.insert_symbol(&make_symbol("B", "fn_b")).unwrap();
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();

        // Notes: N1, N2 (just two flat nodes — enough to verify PPR can
        // load them via the unified scope without errors).
        for (uid, title) in [("note:n1", "One"), ("note:n2", "Two")] {
            store
                .insert_note(&Note {
                    uid: uid.to_string(),
                    vault_uid: "vlt:test".to_string(),
                    file_path: format!("{title}.md"),
                    title: title.to_string(),
                    note_kind: NoteKind::General,
                    word_count: 1,
                    content_hash: "h".to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                })
                .unwrap();
        }

        // Code-only scope: 2 nodes (Symbol A + B).
        let code = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::code_only())
            .unwrap();
        let code_uids: std::collections::HashSet<&str> =
            code.iter().map(|(u, _)| u.as_str()).collect();
        assert!(code_uids.contains("A"));
        assert!(!code_uids.contains("note:n1"));

        // Notes-only scope seeded by a note: should not surface symbols.
        let notes = store
            .personalized_pagerank(
                &["note:n1".to_string()],
                0.85,
                20,
                &GraphScope::notes_only(),
            )
            .unwrap();
        let notes_uids: std::collections::HashSet<&str> =
            notes.iter().map(|(u, _)| u.as_str()).collect();
        assert!(notes_uids.contains("note:n1"));
        assert!(!notes_uids.contains("A"));

        // Unified scope: both kinds visible. Seeding from a symbol still
        // includes the symbol; seeding from a note still includes the note.
        let unified_sym = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::unified())
            .unwrap();
        let unified_sym_uids: std::collections::HashSet<&str> =
            unified_sym.iter().map(|(u, _)| u.as_str()).collect();
        assert!(unified_sym_uids.contains("A"));

        // Unified compute_pagerank produces scores across both domains.
        store
            .compute_pagerank(0.85, 20, &GraphScope::unified())
            .unwrap();
        // Both kinds present in the cache.
        let cache = store.pagerank_cache.lock().unwrap();
        let scores = cache.as_ref().unwrap();
        assert!(scores.contains_key("A"));
        assert!(scores.contains_key("note:n1"));
    }

    #[test]
    fn notes_only_scope_ranks_wikilink_hub_higher() {
        // Build a 3-note vault where note B is wikilinked by both A and C
        // (one section in each note linking to B). PPR seeded from A should
        // rank B above C.
        use nestweaver_schema::{Heading, Note, NoteKind, Section};

        let store = test_store();

        // Notes: A, B, C.
        for (uid, title) in [("note:a", "A"), ("note:b", "B"), ("note:c", "C")] {
            store
                .insert_note(&Note {
                    uid: uid.to_string(),
                    vault_uid: "vlt:v".to_string(),
                    file_path: format!("{title}.md"),
                    title: title.to_string(),
                    note_kind: NoteKind::General,
                    word_count: 10,
                    content_hash: "h".to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                })
                .unwrap();
        }

        // One heading per note (level 1).
        for (n, name) in [("note:a", "A"), ("note:b", "B"), ("note:c", "C")] {
            store
                .insert_heading(&Heading {
                    uid: format!("head:{n}"),
                    note_uid: n.to_string(),
                    level: 1,
                    text: name.to_string(),
                    slug: name.to_lowercase(),
                    start_line: 1,
                    end_line: 1,
                    content_hash: "h".to_string(),
                })
                .unwrap();
        }

        // One section per note, attached to its heading.
        for n in ["note:a", "note:b", "note:c"] {
            store
                .insert_section(&Section {
                    uid: format!("sec:{n}"),
                    note_uid: n.to_string(),
                    heading_uid: Some(format!("head:{n}")),
                    start_line: 2,
                    end_line: 5,
                    text_hash: "t".to_string(),
                    text_content: "body".to_string(),
                    word_count: 5,
                    pagerank_score: None,
                })
                .unwrap();
        }

        // Containment edges so notes_only scope picks them up as connected.
        store
            .batch_insert_note_section_edges(&[
                ("note:a", "sec:note:a"),
                ("note:b", "sec:note:b"),
                ("note:c", "sec:note:c"),
            ])
            .unwrap();

        // Wikilinks: A→B and C→B. B is the hub.
        store
            .batch_insert_wikilink_to_note_edges(&[
                ("sec:note:a", "note:b", 1.0, "B"),
                ("sec:note:c", "note:b", 1.0, "B"),
            ])
            .unwrap();

        // Notes-only PPR seeded from A.
        let results = store
            .personalized_pagerank(&["note:a".to_string()], 0.85, 30, &GraphScope::notes_only())
            .unwrap();

        let score = |uid: &str| {
            results
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0)
        };

        // B is reachable from A (one hop: A's section → B via WIKILINK_TO_NOTE,
        // then containment back to B) and is more central than C (which is
        // only reachable via B→C reverse-wikilink path). B should outrank C.
        let b = score("note:b");
        let c = score("note:c");
        assert!(b > 0.0, "B should have a nonzero PPR score");
        assert!(
            b > c,
            "wikilink-hub note B ({b:.6}) should rank higher than peripheral note C ({c:.6})"
        );
    }

    #[test]
    fn unified_scope_returns_both_symbols_and_notes() {
        use nestweaver_schema::{Note, NoteKind};

        let store = test_store();

        // Code side: A calls B.
        store.insert_symbol(&make_symbol("sym:a", "fn_a")).unwrap();
        store.insert_symbol(&make_symbol("sym:b", "fn_b")).unwrap();
        store
            .insert_edge(&make_calls_edge("sym:a", "sym:b"))
            .unwrap();

        // Notes side: two flat notes.
        for (uid, title) in [("note:p", "P"), ("note:q", "Q")] {
            store
                .insert_note(&Note {
                    uid: uid.to_string(),
                    vault_uid: "vlt:v".to_string(),
                    file_path: format!("{title}.md"),
                    title: title.to_string(),
                    note_kind: NoteKind::General,
                    word_count: 1,
                    content_hash: "h".to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                })
                .unwrap();
        }

        // Unified PPR seeded with one symbol AND one note.
        let results = store
            .personalized_pagerank(
                &["sym:a".to_string(), "note:p".to_string()],
                0.85,
                20,
                &GraphScope::unified(),
            )
            .unwrap();

        let uids: std::collections::HashSet<&str> =
            results.iter().map(|(u, _)| u.as_str()).collect();
        assert!(uids.contains("sym:a"), "symbol seed should appear");
        assert!(uids.contains("note:p"), "note seed should appear");

        // Verify the kind mix: results contain at least one sym:* and one note:*.
        let any_sym = uids.iter().any(|u| u.starts_with("sym:"));
        let any_note = uids.iter().any(|u| u.starts_with("note:"));
        assert!(any_sym, "unified PPR should surface at least one Symbol");
        assert!(any_note, "unified PPR should surface at least one Note");
    }

    #[test]
    fn ppr_conserves_score_mass() {
        // A->B, C is isolated. After dangling-node fix, total PPR score
        // should sum to ~1.0 even with dangling nodes present.
        let store = test_store();
        for uid in ["A", "B", "C"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();

        let results = store
            .personalized_pagerank(&["A".to_string()], 0.85, 100, &GraphScope::code_only())
            .unwrap();
        let total: f64 = results.iter().map(|(_, s)| s).sum();
        assert!(
            (total - 1.0).abs() < 0.01,
            "PPR scores should sum to ~1.0, got {total:.4}"
        );
    }

    #[test]
    fn compute_pagerank_converges_early() {
        let store = test_store();
        for uid in ["A", "B", "C"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();

        // 1000 iterations — should converge well before that.
        store
            .compute_pagerank(0.85, 1000, &GraphScope::code_only())
            .unwrap();
        let ranked = store.symbols_by_pagerank(None).unwrap();
        assert!(!ranked.is_empty());
    }

    #[test]
    fn symbols_by_pagerank_returns_sorted() {
        // Build a small graph, compute pagerank, verify results are descending by score.
        let store = test_store();
        store.insert_symbol(&make_symbol("X", "fn_x")).unwrap();
        store.insert_symbol(&make_symbol("Y", "fn_y")).unwrap();
        store.insert_symbol(&make_symbol("Z", "fn_z")).unwrap();

        // Z has most incoming.
        store.insert_edge(&make_calls_edge("X", "Z")).unwrap();
        store.insert_edge(&make_calls_edge("Y", "Z")).unwrap();

        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();

        let ranked = store.symbols_by_pagerank(None).unwrap();
        assert!(!ranked.is_empty(), "expected at least one ranked symbol");

        let scores: Vec<f64> = ranked
            .iter()
            .map(|s| s.pagerank_score.unwrap_or(0.0))
            .collect();

        for window in scores.windows(2) {
            assert!(
                window[0] >= window[1],
                "scores should be non-increasing: {scores:?}"
            );
        }
    }

    // ── QueryIntent tests ────────────────────────────────────────────────

    #[test]
    fn query_intent_roundtrip_parse() {
        use super::QueryIntent;
        assert_eq!(
            "find-definition".parse::<QueryIntent>().unwrap(),
            QueryIntent::FindDefinition,
        );
        assert_eq!(
            "architecture".parse::<QueryIntent>().unwrap(),
            QueryIntent::UnderstandArchitecture,
        );
        assert_eq!(
            "impact".parse::<QueryIntent>().unwrap(),
            QueryIntent::AnalyzeImpact,
        );
        assert_eq!(
            "general".parse::<QueryIntent>().unwrap(),
            QueryIntent::GeneralContext,
        );
        assert!("nonsense".parse::<QueryIntent>().is_err());
    }

    #[test]
    fn query_intent_damping_values() {
        use super::QueryIntent;
        assert!((QueryIntent::FindDefinition.damping() - 0.5).abs() < f64::EPSILON);
        assert!((QueryIntent::UnderstandArchitecture.damping() - 0.85).abs() < f64::EPSILON);
        assert!((QueryIntent::AnalyzeImpact.damping() - 0.7).abs() < f64::EPSILON);
        assert!((QueryIntent::GeneralContext.damping() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn query_intent_calls_weight_multiplier() {
        use super::QueryIntent;
        assert!((QueryIntent::AnalyzeImpact.calls_weight_multiplier() - 2.0).abs() < f64::EPSILON);
        assert!((QueryIntent::FindDefinition.calls_weight_multiplier() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ppr_with_intent_none_matches_default() {
        // Running PPR with intent=None and the same damping should produce
        // identical results to the original personalized_pagerank.
        let store = test_store();
        for uid in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("D", "C")).unwrap();

        let original = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::code_only())
            .unwrap();
        let with_none = store
            .personalized_pagerank_with_intent(
                &["A".to_string()],
                0.85,
                20,
                &GraphScope::code_only(),
                None,
            )
            .unwrap();

        assert_eq!(
            original.len(),
            with_none.len(),
            "intent=None should produce same result count"
        );
        for ((uid1, s1), (uid2, s2)) in original.iter().zip(with_none.iter()) {
            assert_eq!(uid1, uid2);
            assert!(
                (s1 - s2).abs() < 1e-10,
                "scores should be identical: {s1} vs {s2}"
            );
        }
    }

    #[test]
    fn ppr_find_definition_focuses_on_seed() {
        // FindDefinition (alpha=0.5, d=0.5) concentrates mass on the seed
        // more than the default (d=0.85). The seed A should have a higher
        // proportion of the total score under FindDefinition.
        use super::QueryIntent;

        let store = test_store();
        for uid in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("C", "D")).unwrap();

        let default_results = store
            .personalized_pagerank(&["A".to_string()], 0.85, 40, &GraphScope::code_only())
            .unwrap();
        let focused_results = store
            .personalized_pagerank_with_intent(
                &["A".to_string()],
                0.85,
                40,
                &GraphScope::code_only(),
                Some(QueryIntent::FindDefinition),
            )
            .unwrap();

        let score_of = |results: &[(String, f64)], uid: &str| -> f64 {
            results
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0)
        };

        let default_a = score_of(&default_results, "A");
        let focused_a = score_of(&focused_results, "A");

        // Under FindDefinition (d=0.5), seed A retains a larger fraction
        // of score mass than under the default (d=0.85).
        assert!(
            focused_a > default_a,
            "FindDefinition should concentrate more mass on seed A: \
             focused={focused_a:.6} vs default={default_a:.6}"
        );
    }

    #[test]
    fn ppr_analyze_impact_boosts_calls_edges() {
        // AnalyzeImpact doubles CALLS edge weight. In a graph where A calls B
        // and A imports C (both with confidence 1.0), B should receive more
        // relative score under AnalyzeImpact than under the default.
        use super::QueryIntent;

        let store = test_store();
        for uid in ["A", "B", "C"] {
            let mut sym = make_symbol(uid, &format!("fn_{uid}"));
            sym.file_path = format!("src/{uid}.rs");
            store.insert_symbol(&sym).unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "A".to_string(),
                target_uid: "C".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 1.0,
                link_type: None,
            })
            .unwrap();

        let default_results = store
            .personalized_pagerank(&["A".to_string()], 0.85, 40, &GraphScope::code_only())
            .unwrap();
        let impact_results = store
            .personalized_pagerank_with_intent(
                &["A".to_string()],
                0.85,
                40,
                &GraphScope::code_only(),
                Some(QueryIntent::AnalyzeImpact),
            )
            .unwrap();

        let score_of = |results: &[(String, f64)], uid: &str| -> f64 {
            results
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0)
        };

        let default_b = score_of(&default_results, "B");
        let default_c = score_of(&default_results, "C");
        let impact_b = score_of(&impact_results, "B");
        let impact_c = score_of(&impact_results, "C");

        // Under default, CALLS and IMPORTS have equal weight, so B and C
        // should be roughly equal. Under AnalyzeImpact, CALLS weight is
        // 2x, so B should be boosted relative to C.
        let default_ratio = if default_c > 0.0 {
            default_b / default_c
        } else {
            f64::INFINITY
        };
        let impact_ratio = if impact_c > 0.0 {
            impact_b / impact_c
        } else {
            f64::INFINITY
        };

        assert!(
            impact_ratio > default_ratio,
            "AnalyzeImpact should boost CALLS-reachable B relative to IMPORTS-reachable C: \
             impact ratio={impact_ratio:.4} vs default ratio={default_ratio:.4}"
        );
    }

    #[test]
    fn detect_intent_single_seed() {
        use super::detect_intent;

        let store = test_store();
        store.insert_symbol(&make_symbol("A", "fn_a")).unwrap();

        let intent = detect_intent(&store, &["A".to_string()]);
        assert_eq!(
            intent,
            super::QueryIntent::FindDefinition,
            "single seed should detect FindDefinition"
        );
    }

    #[test]
    fn detect_intent_multiple_files() {
        use super::detect_intent;

        let store = test_store();
        let mut sym_a = make_symbol("A", "fn_a");
        sym_a.file_path = "src/a.rs".to_string();
        let mut sym_b = make_symbol("B", "fn_b");
        sym_b.file_path = "src/b.rs".to_string();
        store.insert_symbol(&sym_a).unwrap();
        store.insert_symbol(&sym_b).unwrap();

        let intent = detect_intent(&store, &["A".to_string(), "B".to_string()]);
        assert_eq!(
            intent,
            super::QueryIntent::UnderstandArchitecture,
            "multiple seeds from different files should detect UnderstandArchitecture"
        );
    }

    #[test]
    fn detect_intent_entry_point() {
        use super::detect_intent;

        let store = test_store();
        let mut sym_a = make_symbol("A", "fn_a");
        sym_a.is_entry_point = true;
        store.insert_symbol(&sym_a).unwrap();

        let intent = detect_intent(&store, &["A".to_string()]);
        assert_eq!(
            intent,
            super::QueryIntent::AnalyzeImpact,
            "entry point seed should detect AnalyzeImpact"
        );
    }

    #[test]
    fn detect_intent_same_file_multiple_seeds() {
        use super::detect_intent;

        let store = test_store();
        // Both seeds from the same file, not entry points.
        let sym_a = make_symbol("A", "fn_a");
        let sym_b = make_symbol("B", "fn_b");
        store.insert_symbol(&sym_a).unwrap();
        store.insert_symbol(&sym_b).unwrap();

        let intent = detect_intent(&store, &["A".to_string(), "B".to_string()]);
        assert_eq!(
            intent,
            super::QueryIntent::GeneralContext,
            "multiple seeds from same file should detect GeneralContext"
        );
    }

    #[test]
    fn detect_intent_empty_seeds() {
        use super::detect_intent;

        let store = test_store();
        let intent = detect_intent(&store, &[]);
        assert_eq!(
            intent,
            super::QueryIntent::GeneralContext,
            "empty seeds should default to GeneralContext"
        );
    }

    #[test]
    fn ppr_with_intent_conserves_score_mass() {
        // Verify that total PPR score sums to ~1.0 even with intent overrides.
        use super::QueryIntent;

        let store = test_store();
        for uid in ["A", "B", "C"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();

        for intent in [
            QueryIntent::FindDefinition,
            QueryIntent::UnderstandArchitecture,
            QueryIntent::AnalyzeImpact,
            QueryIntent::GeneralContext,
            QueryIntent::ProjectContext,
        ] {
            let results = store
                .personalized_pagerank_with_intent(
                    &["A".to_string()],
                    0.85,
                    100,
                    &GraphScope::code_only(),
                    Some(intent),
                )
                .unwrap();
            let total: f64 = results.iter().map(|(_, s)| s).sum();
            assert!(
                (total - 1.0).abs() < 0.01,
                "PPR with intent {:?} should conserve score mass, got {total:.4}",
                intent
            );
        }
    }

    /// Verify that PPR with `ProjectContext` intent ranks project-member notes
    /// above high-in-degree unrelated notes.
    ///
    /// Setup:
    /// - Project "P" with two member notes (low wikilink in-degree).
    /// - Two unrelated notes with many inbound wikilinks (high in-degree).
    /// - PPR seeded from the Project node.
    ///
    /// Without the 5x PROJECT_INCLUDES_* boost the unrelated notes would
    /// dominate the ranking via their superior in-degree. With the boost,
    /// the project's own notes must rank higher.
    #[test]
    fn project_context_intent_ranks_project_members_higher() {
        use nestweaver_schema::{Note, NoteKind, Project, Section, Vault};

        use super::QueryIntent;

        let store = test_store();

        // -- Vault
        let vault = Vault {
            uid: "vlt:test".to_string(),
            name: "TestVault".to_string(),
            root_path: "/tmp/vault".to_string(),
            instance_id: "inst-1".to_string(),
        };
        store.insert_vault(&vault).unwrap();

        // -- Project
        let project = Project {
            uid: "proj:test:P".to_string(),
            name: "ProjectP".to_string(),
            summary: None,
            instance_id: "inst-1".to_string(),
        };
        store.upsert_project(&project).unwrap();

        // -- Project member notes (low in-degree: no wikilinks pointing at them)
        let member_note_a = Note {
            uid: "note:member_a".to_string(),
            vault_uid: "vlt:test".to_string(),
            file_path: "projects/member_a.md".to_string(),
            title: "Member A".to_string(),
            note_kind: NoteKind::General,
            word_count: 50,
            content_hash: "ha".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
        };
        let member_note_b = Note {
            uid: "note:member_b".to_string(),
            vault_uid: "vlt:test".to_string(),
            file_path: "projects/member_b.md".to_string(),
            title: "Member B".to_string(),
            note_kind: NoteKind::General,
            word_count: 50,
            content_hash: "hb".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
        };
        store.upsert_note(&member_note_a).unwrap();
        store.upsert_note(&member_note_b).unwrap();

        // -- Unrelated notes (will have high in-degree from many wikilinks)
        let popular_note_x = Note {
            uid: "note:popular_x".to_string(),
            vault_uid: "vlt:test".to_string(),
            file_path: "general/popular_x.md".to_string(),
            title: "Popular X".to_string(),
            note_kind: NoteKind::General,
            word_count: 200,
            content_hash: "hx".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
        };
        let popular_note_y = Note {
            uid: "note:popular_y".to_string(),
            vault_uid: "vlt:test".to_string(),
            file_path: "general/popular_y.md".to_string(),
            title: "Popular Y".to_string(),
            note_kind: NoteKind::General,
            word_count: 200,
            content_hash: "hy".to_string(),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
        };
        store.upsert_note(&popular_note_x).unwrap();
        store.upsert_note(&popular_note_y).unwrap();

        // -- Create several "filler" notes that all wikilink to popular_x and
        //    popular_y, giving them high in-degree.
        for i in 0..8 {
            let filler_uid = format!("note:filler_{i}");
            let sec_uid = format!("sec:filler_{i}:body");
            let filler = Note {
                uid: filler_uid.clone(),
                vault_uid: "vlt:test".to_string(),
                file_path: format!("fillers/filler_{i}.md"),
                title: format!("Filler {i}"),
                note_kind: NoteKind::General,
                word_count: 30,
                content_hash: format!("hf{i}"),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
            };
            store.upsert_note(&filler).unwrap();

            let section = Section {
                uid: sec_uid.clone(),
                note_uid: filler_uid.clone(),
                heading_uid: None,
                start_line: 1,
                end_line: 5,
                text_hash: format!("th{i}"),
                text_content: format!("see [[Popular X]] and [[Popular Y]]"),
                word_count: 6,
                pagerank_score: None,
            };
            store.insert_section(&section).unwrap();
            store
                .batch_insert_note_section_edges(&[(&filler_uid, &sec_uid)])
                .unwrap();

            // Wikilink from filler section -> popular_x and popular_y
            store
                .batch_insert_wikilink_to_note_edges(&[
                    (&sec_uid, "note:popular_x", 1.0, "Popular X"),
                    (&sec_uid, "note:popular_y", 1.0, "Popular Y"),
                ])
                .unwrap();
        }

        // -- Link project to its member notes
        store
            .batch_insert_project_note_edges(&[
                ("proj:test:P", "note:member_a"),
                ("proj:test:P", "note:member_b"),
            ])
            .unwrap();

        // -- PPR from project node with ProjectContext intent
        let project_results = store
            .personalized_pagerank_with_intent(
                &["proj:test:P".to_string()],
                0.85,
                100,
                &GraphScope::unified(),
                Some(QueryIntent::ProjectContext),
            )
            .unwrap();

        // Find scores for member notes vs popular notes.
        let score_of = |uid: &str| -> f64 {
            project_results
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0)
        };

        let member_a_score = score_of("note:member_a");
        let member_b_score = score_of("note:member_b");
        let popular_x_score = score_of("note:popular_x");
        let popular_y_score = score_of("note:popular_y");

        // With the 5x PROJECT_INCLUDES_* boost, project member notes should
        // rank above the popular unrelated notes.
        assert!(
            member_a_score > popular_x_score,
            "Project member A ({member_a_score:.6}) should rank above popular X ({popular_x_score:.6})"
        );
        assert!(
            member_b_score > popular_y_score,
            "Project member B ({member_b_score:.6}) should rank above popular Y ({popular_y_score:.6})"
        );

        // -- Verify the boost has a material effect: with GeneralContext the
        //    member-to-popular score ratio should be worse (popular notes
        //    benefit more from their in-degree without the project edge boost).
        let general_results = store
            .personalized_pagerank_with_intent(
                &["proj:test:P".to_string()],
                0.85,
                100,
                &GraphScope::unified(),
                Some(QueryIntent::GeneralContext),
            )
            .unwrap();

        let general_score_of = |uid: &str| -> f64 {
            general_results
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0)
        };

        let gen_popular_x = general_score_of("note:popular_x");
        let gen_member_a = general_score_of("note:member_a");

        // The ratio member/popular should be better with ProjectContext than
        // with GeneralContext. Use safe division for the general case (popular
        // might be zero if unreachable).
        let project_ratio = if popular_x_score > 0.0 {
            member_a_score / popular_x_score
        } else {
            f64::INFINITY
        };
        let general_ratio = if gen_popular_x > 0.0 {
            gen_member_a / gen_popular_x
        } else {
            // If popular notes are unreachable in both intents, the boost still
            // did its job (member notes are on top regardless).
            f64::INFINITY
        };
        assert!(
            project_ratio >= general_ratio,
            "ProjectContext member/popular ratio ({project_ratio:.4}) should be >= \
             GeneralContext ratio ({general_ratio:.4})"
        );
    }
}
