//! PageRank and personalized-PageRank ranking over the graph: computation,
//! the in-memory caches, and the `.pagerank.json` sidecar.
//!
//! # Dirty-publication contract
//!
//! While an index publication is in flight (the `<db>.index-dirty` marker
//! exists — see `crate::index_publication`), the ranking caches may describe
//! a graph that no longer exists. Every guard site in this module calls
//! `invalidate_ranking_caches_locked` first, so cache correctness is uniform.
//! The return contract is uniform by policy:
//!
//! - **Query paths fail closed.** `compute_pagerank`, `personalized_pagerank*`,
//!   `symbols_by_pagerank`, `pagerank_scores`, `ensure_pagerank_loaded`, and
//!   `warm_ppr_cache` return `Err(StoreError::RankingUnavailable)` ("PageRank
//!   unavailable during dirty index publication"). A publication window must
//!   never be mistaken for an empty graph, so no query path answers with a
//!   successful-looking empty result.
//! - **Sidecar load/save no-op.** `save_pagerank_cache` and
//!   `load_pagerank_cache` return `Ok(())` without reading or writing sidecar
//!   data (the caches are still invalidated, as at every guard site):
//!   refusing to persist or load during the window is a no-op by nature, not
//!   a degraded answer.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::Write;

use lbug::Value;
use nestweaver_algorithms::graph::AdjacencyData;
use nestweaver_algorithms::ppr::{PprConfig, forward_push_ppr};
use nestweaver_schema::{EdgeType, Symbol};
use serde::{Deserialize, Serialize};

use crate::db::{GraphStore, PprGraphCached};
use crate::error::StoreError;
use crate::read::{SYMBOL_COLUMNS, row_to_symbol};

pub const PAGERANK_ARTIFACT_KIND: &str = "ranking";
pub const PAGERANK_ARTIFACT_SCHEMA_VERSION: u32 = 2;
pub const PAGERANK_ALGORITHM_FINGERPRINT_PREFIX: &str = "nestweaver-pagerank-v2:";

fn pagerank_algorithm_fingerprint(damping: f64, iterations: u32, scope: &GraphScope) -> String {
    let mut digest = blake3::Hasher::new();
    digest.update(b"nestweaver-pagerank-scope-v2\0");
    digest.update(&damping.to_bits().to_le_bytes());
    digest.update(&iterations.to_le_bytes());
    for query in &scope.node_queries {
        digest.update(&(query.len() as u64).to_le_bytes());
        digest.update(query.as_bytes());
    }
    for edge in &scope.edge_queries {
        digest.update(&(edge.query.len() as u64).to_le_bytes());
        digest.update(edge.query.as_bytes());
        digest.update(
            edge.edge_type
                .map(|kind| kind.rel_table_name())
                .unwrap_or("<structural>")
                .as_bytes(),
        );
        digest.update(&[0]);
    }
    format!(
        "{PAGERANK_ALGORITHM_FINGERPRINT_PREFIX}{}",
        digest.finalize().to_hex()
    )
}

/// Feature F12 default activity weight. With `score ∈ [0, 1]`, the factor
/// `1 + w*(score - 0.5)` spans `[1 - w/2, 1 + w/2]`; `w = 1.2` is required for
/// it to reach the intended `[0.4, 1.6]` clamp (the RFC's `0.6` only reaches
/// `[0.7, 1.3]`). See `nestweaver_engine::git_activity` for the full rationale.
pub const DEFAULT_GIT_ACTIVITY_WEIGHT: f64 = 1.2;

/// Lower clamp bound for the git-activity rank-read multiplier.
pub const GIT_ACTIVITY_MULT_MIN: f64 = 0.4;
/// Upper clamp bound for the git-activity rank-read multiplier.
pub const GIT_ACTIVITY_MULT_MAX: f64 = 1.6;

/// Feature F12 rank-read multiplier applied to a `pagerank_score`.
///
/// - `score == None` → neutral `1.0` (no recency data for this file).
/// - `score == Some(s)` → `clamp(1 + weight * (s - 0.5), 0.4, 1.6)`.
///
/// The result is always within `[0.4, 1.6]`. This is the source of truth the
/// store applies; `nestweaver_engine::hubs` and the miner mirror the same
/// formula (the engine crate sits above the store and cannot be depended on
/// here, so the constant/formula is duplicated and kept in sync by tests).
pub fn git_activity_multiplier(score: Option<f64>, weight: f64) -> f64 {
    match score {
        None => 1.0,
        Some(s) => (1.0 + weight * (s - 0.5)).clamp(GIT_ACTIVITY_MULT_MIN, GIT_ACTIVITY_MULT_MAX),
    }
}

/// Lower / upper bounds applied to a [`PathDeboostRule::factor`] on load.
pub const SEED_PATH_FACTOR_MIN: f64 = 0.0;
pub const SEED_PATH_FACTOR_MAX: f64 = 10.0;

/// Controls how seed candidates are scored before PPR expansion.
///
/// Use this block to deboost candidates whose `file_path` lives in test
/// mirrors (e.g. `playwright/`, `__tests__/`) so production symbols win
/// when names collide. Multiple matching rules multiply.
///
/// Defaults target JS/TS test mirror paths (`playwright/`, `__tests__/`,
/// `*.test.ts`, etc.). Tune per-project by adding to
/// `[seed_resolution].path_deboost` in `nestweaver-instance.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedResolutionConfig {
    /// Multiplicative factor applied to a seed candidate's match score
    /// when its file_path matches the given prefix or suffix. Factor < 1.0
    /// deboosts; factor > 1.0 boosts. Multiple matches multiply.
    #[serde(default)]
    pub path_deboost: Vec<PathDeboostRule>,
    /// When candidates tie on path-factor-adjusted score, prefer the kind
    /// earlier in this list. Earlier = higher priority.
    #[serde(default = "default_kind_priority")]
    pub kind_priority: Vec<String>,
}

/// A single seed-resolution path deboost / boost rule.
///
/// Exactly one of `prefix` / `suffix` must be set. Validation in the engine
/// layer rejects rules that violate this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathDeboostRule {
    /// Match if the (`/`-prepended, lowercased) file_path contains this
    /// substring. Mutually exclusive with `suffix`.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Match if file_path ends with this string (case-sensitive).
    /// Mutually exclusive with `prefix`.
    #[serde(default)]
    pub suffix: Option<String>,
    /// Multiplier on the candidate's score. Clamped to
    /// `[SEED_PATH_FACTOR_MIN, SEED_PATH_FACTOR_MAX]` on load.
    pub factor: f64,
}

/// Default priority list for tie-breaking seed candidates by `SymbolKind`.
///
/// Earlier entries win against later ones. The list covers every variant
/// of [`nestweaver_schema::SymbolKind`] so any well-typed symbol has a
/// defined rank.
pub fn default_kind_priority() -> Vec<String> {
    vec![
        "Class".into(),
        "Interface".into(),
        "TypeAlias".into(),
        "Method".into(),
        "Function".into(),
        "Constant".into(),
        "Property".into(),
        "Variable".into(),
        "Module".into(),
        "Enum".into(),
        "Trait".into(),
        "Extension".into(),
    ]
}

impl Default for SeedResolutionConfig {
    fn default() -> Self {
        Self {
            path_deboost: vec![
                PathDeboostRule {
                    prefix: Some("/playwright/".into()),
                    suffix: None,
                    factor: 0.2,
                },
                PathDeboostRule {
                    prefix: Some("/__tests__/".into()),
                    suffix: None,
                    factor: 0.3,
                },
                PathDeboostRule {
                    prefix: Some("/cypress/".into()),
                    suffix: None,
                    factor: 0.3,
                },
                PathDeboostRule {
                    prefix: Some("/e2e/".into()),
                    suffix: None,
                    factor: 0.3,
                },
                PathDeboostRule {
                    prefix: Some("/tests/".into()),
                    suffix: None,
                    factor: 0.3,
                },
                PathDeboostRule {
                    prefix: Some("/test/".into()),
                    suffix: None,
                    factor: 0.3,
                },
                PathDeboostRule {
                    prefix: Some("/spec/".into()),
                    suffix: None,
                    factor: 0.3,
                },
                PathDeboostRule {
                    prefix: Some("/__mocks__/".into()),
                    suffix: None,
                    factor: 0.3,
                },
                PathDeboostRule {
                    prefix: Some("/fixtures/".into()),
                    suffix: None,
                    factor: 0.4,
                },
                // Vendored test-runtime packages bundled into a repo's
                // dependencies. These leak into seed candidates and PPR walks
                // when third-party test packages ship their own framework code
                // under the repo tree.
                PathDeboostRule {
                    prefix: Some("/jasmine/".into()),
                    suffix: None,
                    factor: 0.2,
                },
                PathDeboostRule {
                    prefix: Some("/mocha/".into()),
                    suffix: None,
                    factor: 0.2,
                },
                PathDeboostRule {
                    prefix: Some("/qunit/".into()),
                    suffix: None,
                    factor: 0.2,
                },
                PathDeboostRule {
                    prefix: Some("/jest/".into()),
                    suffix: None,
                    factor: 0.2,
                },
                PathDeboostRule {
                    prefix: Some("/karma/".into()),
                    suffix: None,
                    factor: 0.2,
                },
                PathDeboostRule {
                    prefix: None,
                    suffix: Some(".test.tsx".into()),
                    factor: 0.5,
                },
                PathDeboostRule {
                    prefix: None,
                    suffix: Some(".test.ts".into()),
                    factor: 0.5,
                },
                PathDeboostRule {
                    prefix: None,
                    suffix: Some(".test.jsx".into()),
                    factor: 0.5,
                },
                PathDeboostRule {
                    prefix: None,
                    suffix: Some(".test.js".into()),
                    factor: 0.5,
                },
                PathDeboostRule {
                    prefix: None,
                    suffix: Some(".spec.ts".into()),
                    factor: 0.5,
                },
            ],
            kind_priority: default_kind_priority(),
        }
    }
}

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
                 understand-architecture, analyze-impact, blast-radius, \
                 general-context, project-context",
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

/// A single edge query paired with an optional `EdgeType` tag so downstream
/// consumers (e.g. `load_ppr_graph`) can classify edges without parsing the
/// Cypher query string.
#[derive(Debug, Clone)]
pub struct ScopedEdgeQuery {
    pub query: String,
    pub edge_type: Option<EdgeType>,
}

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
    pub edge_queries: Vec<ScopedEdgeQuery>,
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
                .map(|et| ScopedEdgeQuery {
                    query: format!(
                        "MATCH (a:Symbol)-[r:{}]->(b:Symbol) RETURN a.uid, b.uid, r.confidence",
                        et.rel_table_name()
                    ),
                    edge_type: Some(*et),
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
                ScopedEdgeQuery { query: "MATCH (a:Note)-[:NOTE_HAS_HEADING]->(b:Heading) RETURN a.uid, b.uid".to_string(), edge_type: None },
                ScopedEdgeQuery { query: "MATCH (a:Note)-[:NOTE_HAS_SECTION]->(b:Section) RETURN a.uid, b.uid".to_string(), edge_type: None },
                ScopedEdgeQuery { query: "MATCH (a:Heading)-[:HEADING_HAS_SECTION]->(b:Section) RETURN a.uid, b.uid".to_string(), edge_type: None },
                ScopedEdgeQuery { query: "MATCH (a:Heading)-[:HEADING_PARENT]->(b:Heading) RETURN a.uid, b.uid".to_string(), edge_type: None },
                // Wikilinks carry confidence — query the property explicitly.
                ScopedEdgeQuery { query: "MATCH (a:Section)-[r:WIKILINK_TO_NOTE]->(b:Note) RETURN a.uid, b.uid, r.confidence".to_string(), edge_type: None },
                ScopedEdgeQuery { query: "MATCH (a:Section)-[r:WIKILINK_TO_HEADING]->(b:Heading) RETURN a.uid, b.uid, r.confidence".to_string(), edge_type: None },
                // Tag edges: no confidence property → defaults to 1.0.
                ScopedEdgeQuery { query: "MATCH (a:Note)-[:NOTE_TAGGED_WITH]->(b:Tag) RETURN a.uid, b.uid".to_string(), edge_type: None },
                ScopedEdgeQuery { query: "MATCH (a:Section)-[:SECTION_TAGGED_WITH]->(b:Tag) RETURN a.uid, b.uid".to_string(), edge_type: None },
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
        scope.edge_queries.push(ScopedEdgeQuery {
            query: "MATCH (a:Note)-[r:REFERENCES_CODE_NOTE_TO_SYMBOL]->(b:Symbol) RETURN a.uid, b.uid, r.confidence".to_string(),
            edge_type: None,
        });
        scope.edge_queries.push(ScopedEdgeQuery {
            query: "MATCH (a:Section)-[r:REFERENCES_CODE_SECTION_TO_SYMBOL]->(b:Symbol) RETURN a.uid, b.uid, r.confidence".to_string(),
            edge_type: None,
        });
        // Project nodes and edges — allows PPR to traverse project membership.
        scope
            .node_queries
            .push("MATCH (p:Project) RETURN p.uid".to_string());
        scope.edge_queries.push(ScopedEdgeQuery {
            query: "MATCH (p:Project)-[r:PROJECT_INCLUDES_NOTE]->(n:Note) RETURN p.uid, n.uid, r.confidence".to_string(),
            edge_type: Some(EdgeType::ProjectIncludesNote),
        });
        scope.edge_queries.push(ScopedEdgeQuery {
            query: "MATCH (p:Project)-[r:PROJECT_INCLUDES_SYMBOL]->(s:Symbol) RETURN p.uid, s.uid, r.confidence".to_string(),
            edge_type: Some(EdgeType::ProjectIncludesSymbol),
        });
        scope.edge_queries.push(ScopedEdgeQuery {
            query: "MATCH (p:Project)-[r:PROJECT_HAS_COMPONENT]->(q:Project) RETURN p.uid, q.uid, r.confidence".to_string(),
            edge_type: Some(EdgeType::ProjectHasComponent),
        });
        scope
    }
}

/// Compute a stable `u64` hash over the query strings in a `GraphScope`.
///
/// The hash encodes the scope identity cheaply so the PPR graph cache can
/// detect scope changes without storing or comparing the full query strings.
fn scope_hash(scope: &GraphScope) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    scope.node_queries.len().hash(&mut hasher);
    for q in &scope.node_queries {
        q.hash(&mut hasher);
    }
    scope.edge_queries.len().hash(&mut hasher);
    for eq in &scope.edge_queries {
        eq.query.hash(&mut hasher);
    }
    hasher.finish()
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
        self.compute_pagerank_warm(damping, iterations, scope, None)
    }

    /// Compute PageRank with optional warm-start from previous scores.
    /// When `warm_start` is provided, known nodes initialize from their
    /// previous score instead of uniform — convergence is faster when only
    /// a small fraction of nodes/edges changed.
    pub fn compute_pagerank_warm(
        &self,
        damping: f64,
        iterations: u32,
        scope: &GraphScope,
        warm_start: Option<&HashMap<String, f64>>,
    ) -> Result<(), StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.compute_pagerank_warm_locked(damping, iterations, scope, warm_start)
    }

    fn compute_pagerank_warm_locked(
        &self,
        damping: f64,
        iterations: u32,
        scope: &GraphScope,
        warm_start: Option<&HashMap<String, f64>>,
    ) -> Result<(), StoreError> {
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Err(StoreError::RankingUnavailable);
        }
        self.compute_pagerank_warm_inner(damping, iterations, scope, warm_start, false)
    }

    /// Compute PageRank while the caller's OWN index publication is still
    /// dirty. [`IndexPublicationLease::compute_pagerank`] is the only caller —
    /// it token-validates ownership first, so this exemption cannot be reached
    /// by an outsider. The guards this skips exist to stop readers from
    /// ranking (or cache-publishing) state that predates an in-flight commit;
    /// the owner ranks the graph it just COMMITTED, which is precisely the
    /// state the fresh sidecar must hold before the marker may retire.
    ///
    /// [`IndexPublicationLease::compute_pagerank`]: crate::db::IndexPublicationLease::compute_pagerank
    pub(crate) fn compute_pagerank_for_publication_owner(
        &self,
        damping: f64,
        iterations: u32,
        scope: &GraphScope,
    ) -> Result<(), StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.compute_pagerank_warm_inner(damping, iterations, scope, None, true)
    }

    /// `for_publication_owner` skips the mid-compute dirty re-check: the
    /// owner's own marker is present throughout, so the re-check would always
    /// fire, and no other publication can begin while the owner holds the
    /// lease. Outsider computes keep the re-check.
    fn compute_pagerank_warm_inner(
        &self,
        damping: f64,
        iterations: u32,
        scope: &GraphScope,
        warm_start: Option<&HashMap<String, f64>>,
        for_publication_owner: bool,
    ) -> Result<(), StoreError> {
        let (uids, _uid_to_idx, incoming, out_weight) = self.load_ppr_graph(scope, None)?;
        let n = uids.len();
        if n == 0 {
            *self
                .pagerank_cache
                .lock()
                .map_err(|e| StoreError::Query(format!("lock: {e}")))? = Some(HashMap::new());
            *self
                .pagerank_artifact_fingerprint
                .lock()
                .map_err(|e| StoreError::Query(format!("lock: {e}")))? =
                Some(pagerank_algorithm_fingerprint(damping, iterations, scope));
            self.bump_pagerank_generation();
            return Ok(());
        }

        let init = 1.0f64 / n as f64;
        let mut scores: Vec<f64> = match warm_start {
            Some(prev) => {
                let mut s = Vec::with_capacity(n);
                let mut warm_count = 0usize;
                for uid in &uids {
                    if let Some(&prev_score) = prev.get(uid) {
                        s.push(prev_score);
                        warm_count += 1;
                    } else {
                        s.push(init);
                    }
                }
                if warm_count > 0 {
                    // Normalize so scores sum to 1.0 (graph size may have changed).
                    let sum: f64 = s.iter().sum();
                    if sum > 0.0 {
                        for v in s.iter_mut() {
                            *v /= sum;
                        }
                    }
                    tracing::info!(
                        warm_count,
                        total = n,
                        "PageRank warm-started from {warm_count}/{n} previous scores"
                    );
                }
                s
            }
            None => vec![init; n],
        };
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

        if !for_publication_owner && self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Err(StoreError::RankingUnavailable);
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
        *self
            .pagerank_artifact_fingerprint
            .lock()
            .map_err(|e| StoreError::Query(format!("lock: {e}")))? =
            Some(pagerank_algorithm_fingerprint(damping, iterations, scope));

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
    /// is identified from the `ScopedEdgeQuery::edge_type` tag carried by
    /// each query.
    ///
    /// Both forward and reverse directions are included so that PPR propagates
    /// relevance through the full neighbourhood.
    pub(crate) fn load_ppr_graph(
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
        for scoped_eq in &scope.edge_queries {
            let q = &scoped_eq.query;
            // Use the typed EdgeType tag to determine base weight and intent
            // multipliers. This avoids fragile substring matching on the query
            // string and stays in sync with EdgeType::rel_table_name().
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
            let (base_weight, intent_multiplier) = match scoped_eq.edge_type {
                Some(EdgeType::Calls) => (1.0, calls_multiplier),
                Some(EdgeType::Extends) | Some(EdgeType::Implements) => (0.9, 1.0),
                Some(EdgeType::Imports) => (0.7, 1.0),
                Some(EdgeType::Uses) => (0.5, 1.0),
                Some(EdgeType::Accesses) => (0.4, 1.0),
                Some(EdgeType::MemberOf) | Some(EdgeType::Includes) => (0.2, 1.0),
                Some(EdgeType::ProjectIncludesNote) | Some(EdgeType::ProjectIncludesSymbol) => {
                    (1.0, project_includes_multiplier)
                }
                _ => (1.0, 1.0),
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
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Err(StoreError::RankingUnavailable);
        }
        let effective_damping = intent.map_or(damping, |i| i.damping());

        // --- PPR graph cache check -------------------------------------------
        // Read the current generation before locking the cache so we never hold
        // the mutex across the (potentially expensive) DB queries.
        let current_gen = self.graph_generation();
        let s_hash = scope_hash(scope);

        // PPR result cache: compute a key from all inputs that affect the result,
        // including the interaction cache content so that loading new interaction
        // scores correctly bypasses the cache.
        let interaction_scores_for_key = self
            .interaction_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let ppr_cache_key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            let mut sorted_seeds = seed_uids.to_vec();
            sorted_seeds.sort();
            sorted_seeds.hash(&mut hasher);
            damping.to_bits().hash(&mut hasher);
            max_iterations.hash(&mut hasher);
            s_hash.hash(&mut hasher);
            intent.hash(&mut hasher);
            current_gen.hash(&mut hasher);
            // Hash interaction scores so cache is invalidated when they change.
            if let Some(ref scores) = interaction_scores_for_key {
                let mut sorted_scores: Vec<(&String, u64)> =
                    scores.iter().map(|(k, v)| (k, v.to_bits())).collect();
                sorted_scores.sort_by_key(|(k, _)| k.as_str());
                sorted_scores.hash(&mut hasher);
            }
            hasher.finish()
        };

        {
            let mut cache = self
                .ppr_result_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.get(&ppr_cache_key) {
                return Ok(cached.clone());
            }
        }

        // Step 1: check cache (lock, compare key, unlock).
        let cache_hit = {
            let guard = self
                .ppr_graph_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = guard.as_ref() {
                cached.generation == current_gen
                    && cached.scope_hash == s_hash
                    && cached.intent == intent
            } else {
                false
            }
        };

        // Step 2: on miss, build the graph (no mutex held during DB I/O).
        if !cache_hit {
            let (uids, uid_to_idx, incoming, out_weight) = self.load_ppr_graph(scope, intent)?;
            let mut guard = self
                .ppr_graph_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = Some(std::sync::Arc::new(PprGraphCached {
                generation: current_gen,
                scope_hash: s_hash,
                intent,
                uids,
                adjacency: AdjacencyData {
                    uid_to_idx,
                    incoming,
                    out_weight,
                },
            }));
        }

        // Step 3: read from cache (guaranteed populated).
        //
        // nw-180: take an Arc handle rather than deep-copying. This path used to
        // clone a corpus-sized HashMap, Vec<String> and edge list on EVERY call
        // -- including cache HITS -- purely to scope the lock, which put a ~2s
        // floor under every brain_context for ~5 KB of output. forward_push_ppr
        // borrows both, so a refcount bump is all that is needed to release the
        // lock before the iterative computation.
        let cached = {
            let guard = self
                .ppr_graph_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::sync::Arc::clone(
                guard
                    .as_ref()
                    .expect("ppr_graph_cache must be Some after fill"),
            )
        };

        // Use the interaction scores already read for the cache key above.
        let interaction_scores = interaction_scores_for_key;

        let config = PprConfig {
            damping: effective_damping,
            max_iterations,
            min_score: 1e-4,
            interaction_scores,
            interaction_bias_weight: 0.05,
        };

        let results = forward_push_ppr(&cached.uids, &cached.adjacency, seed_uids, &config);

        {
            let mut cache = self
                .ppr_result_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            cache.put(ppr_cache_key, results.clone());
        }

        Ok(results)
    }

    /// Return all Symbol nodes that have a pagerank_score set, ordered descending by score.
    ///
    /// Scores are read from the in-memory cache populated by `compute_pagerank`.
    /// If the cache is empty it is computed lazily on first access.
    /// If `limit` is `None`, all symbols are returned.
    /// Fails closed with `Err` during a dirty index publication (see the
    /// module-header contract).
    pub fn symbols_by_pagerank(&self, limit: Option<usize>) -> Result<Vec<Symbol>, StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Err(StoreError::RankingUnavailable);
        }
        self.ensure_pagerank_loaded_locked()?;
        let scores = self
            .pagerank_cache
            .lock()
            .map_err(|e| StoreError::Query(format!("lock: {e}")))?
            .clone();
        let scores = match scores {
            Some(scores) => scores,
            None => return Ok(Vec::new()),
        };

        // Get all symbols, attach scores from cache, sort descending.
        let conn = self.conn()?;
        let q = format!("MATCH (s:Symbol) RETURN {}", SYMBOL_COLUMNS);
        let result = conn
            .query(&q)
            .map_err(|e| StoreError::Query(e.to_string()))?;

        // Feature F12: when git-activity recency scores are loaded, demote
        // dormant code at read time by multiplying the base pagerank by a
        // clamped per-file recency factor. Files with no score → neutral.
        // The PPR fixpoint is untouched — this is purely a read-time rescale.
        let git_activity = self.git_activity_cache.lock().ok().and_then(|g| g.clone());
        let ga_weight = self.git_activity_weight();

        let mut symbols: Vec<Symbol> = result
            .filter_map(|row| row_to_symbol(&row).ok())
            .filter_map(|mut sym| {
                scores.get(&sym.uid).copied().map(|score| {
                    let effective = match &git_activity {
                        Some(ga) => {
                            let mult =
                                git_activity_multiplier(ga.get(&sym.file_path).copied(), ga_weight);
                            score * mult
                        }
                        None => score,
                    };
                    sym.pagerank_score = Some(effective);
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
    /// If the final parent-directory sync fails, the complete new cache may
    /// already be canonical even though crash durability of the rename could
    /// not be confirmed.
    pub fn save_pagerank_cache(&self, path: &std::path::Path) -> Result<(), StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Ok(());
        }
        self.save_pagerank_cache_inner(path, self.graph_generation())
    }

    /// Persist the PageRank sidecar while the caller's OWN index publication
    /// is still dirty. [`IndexPublicationLease::save_pagerank`] is the only
    /// caller and token-validates ownership first. The guarded public variant
    /// refuses to persist mid-publication state for outsiders; the owner
    /// persists ranks over the graph it just COMMITTED, which is what lets
    /// the sidecar land before the marker retires.
    ///
    /// Fails closed on an empty cache. Compute and save are separate calls
    /// under `pagerank_compute_lock`, and any reader touching the rank path
    /// while the marker exists wipes the cache via
    /// `invalidate_ranking_caches_locked` — so a `None` cache here means the
    /// owner's fresh ranks were wiped mid-window (the compute itself always
    /// leaves `Some`, even for an empty graph). Persisting nothing and
    /// returning `Ok` would let the marker retire with no sidecar: the exact
    /// clean-but-rankless state the ordering exists to prevent. The error
    /// blocks retirement; the publication stays dirty and recovers next open.
    ///
    /// [`IndexPublicationLease::save_pagerank`]: crate::db::IndexPublicationLease::save_pagerank
    pub(crate) fn save_pagerank_cache_for_publication_owner(
        &self,
        path: &std::path::Path,
        clean_generation: u64,
    ) -> Result<(), StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self
            .pagerank_cache
            .lock()
            .map_err(|e| StoreError::Query(format!("lock: {e}")))?
            .is_none()
        {
            return Err(StoreError::Query(
                "PageRank cache wiped between the owner's compute and sidecar save; \
                 refusing to publish clean without a persisted sidecar"
                    .into(),
            ));
        }
        self.save_pagerank_cache_inner(path, clean_generation)
    }

    fn save_pagerank_cache_inner(
        &self,
        path: &std::path::Path,
        source_graph_generation: u64,
    ) -> Result<(), StoreError> {
        let cache = self
            .pagerank_cache
            .lock()
            .map_err(|e| StoreError::Query(format!("lock: {e}")))?;
        if let Some(scores) = cache.as_ref() {
            let fingerprint = self
                .pagerank_artifact_fingerprint
                .lock()
                .map_err(|e| StoreError::Query(format!("lock: {e}")))?
                .clone()
                .ok_or_else(|| {
                    StoreError::Query(
                        "PageRank scores have no algorithm fingerprint; recompute before saving"
                            .to_string(),
                    )
                })?;
            let identity = self.publication_identity()?.ok_or_else(|| {
                StoreError::Query(
                    "database has no publication identity; refusing to persist an unbound PageRank artifact"
                        .to_string(),
                )
            })?;
            let envelope = crate::artifact_envelope::ArtifactEnvelope::new(
                crate::artifact_envelope::ArtifactExpectation {
                    artifact_kind: PAGERANK_ARTIFACT_KIND,
                    artifact_schema_version: PAGERANK_ARTIFACT_SCHEMA_VERSION,
                    identity: &identity,
                    producer_version: env!("CARGO_PKG_VERSION"),
                    source_graph_generation,
                    algorithm_fingerprint: &fingerprint,
                },
                scores,
            )?;
            let json = serde_json::to_vec_pretty(&envelope)
                .map_err(|e| StoreError::Query(format!("serialize PageRank artifact: {e}")))?;
            crate::durable_sidecar::atomic_replace_file(path, |file| file.write_all(&json))
                .map_err(|e| StoreError::Query(format!("write: {e}")))?;
        }
        Ok(())
    }

    /// Load the PageRank cache from a JSON sidecar file at `path`.
    ///
    /// If the file does not exist, this is a no-op.
    pub fn load_pagerank_cache(&self, path: &std::path::Path) -> Result<(), StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Ok(());
        }
        if path.exists() {
            // A replacement load is fail-closed: no previously admitted ranks
            // may survive if the new file proves stale, foreign, incompatible,
            // or corrupt at any later validation stage.
            self.invalidate_ranking_caches_locked();
            let json = std::fs::read(path).map_err(|e| StoreError::Query(format!("read: {e}")))?;
            let envelope: crate::artifact_envelope::ArtifactEnvelope =
                serde_json::from_slice(&json).map_err(|error| {
                    StoreError::Query(format!(
                        "incompatible PageRank sidecar {}: expected a self-describing v2 artifact envelope; run a full reindex ({error})",
                        path.display()
                    ))
                })?;
            if !envelope
                .algorithm_fingerprint
                .starts_with(PAGERANK_ALGORITHM_FINGERPRINT_PREFIX)
            {
                return Err(StoreError::Query(format!(
                    "incompatible PageRank algorithm fingerprint '{}'; run a full reindex",
                    envelope.algorithm_fingerprint
                )));
            }
            let identity = self.publication_identity()?.ok_or_else(|| {
                StoreError::Query(
                    "database has no publication identity; refusing to load an unbound PageRank artifact"
                        .to_string(),
                )
            })?;
            let fingerprint = envelope.algorithm_fingerprint.clone();
            let scores: HashMap<String, f64> =
                envelope.validate_and_decode(crate::artifact_envelope::ArtifactExpectation {
                    artifact_kind: PAGERANK_ARTIFACT_KIND,
                    artifact_schema_version: PAGERANK_ARTIFACT_SCHEMA_VERSION,
                    identity: &identity,
                    producer_version: env!("CARGO_PKG_VERSION"),
                    source_graph_generation: self.graph_generation(),
                    algorithm_fingerprint: &fingerprint,
                })?;
            *self
                .pagerank_cache
                .lock()
                .map_err(|e| StoreError::Query(format!("lock: {e}")))? = Some(scores);
            *self
                .pagerank_artifact_fingerprint
                .lock()
                .map_err(|e| StoreError::Query(format!("lock: {e}")))? = Some(fingerprint);
        }
        Ok(())
    }

    /// Return a clone of the in-memory PageRank scores map.
    ///
    /// Returns an empty map if PageRank has not been computed yet or the
    /// cache is not loaded. Used by downstream crates (engine) that need
    /// per-UID score lookups without loading full Symbol objects.
    /// Fails closed with `Err` during a dirty index publication (see the
    /// module-header contract).
    pub fn pagerank_scores(&self) -> Result<HashMap<String, f64>, StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Err(StoreError::RankingUnavailable);
        }
        self.ensure_pagerank_loaded_locked()?;
        Ok(self
            .pagerank_cache
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_default())
    }

    /// Ensure the in-memory PageRank cache is populated.
    ///
    /// If the cache is already loaded (from a sidecar file or a previous
    /// computation), this is a no-op.  Otherwise it computes PageRank on
    /// demand so callers never see an empty cache after a fresh index.
    /// Fails closed with `Err` during a dirty index publication (see the
    /// module-header contract).
    pub fn ensure_pagerank_loaded(&self) -> Result<(), StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ensure_pagerank_loaded_locked()
    }

    fn ensure_pagerank_loaded_locked(&self) -> Result<(), StoreError> {
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Err(StoreError::RankingUnavailable);
        }
        let loaded = |cache: &std::sync::Mutex<Option<HashMap<String, f64>>>| {
            cache.lock().map(|c| c.is_some()).unwrap_or(false)
        };
        if loaded(&self.pagerank_cache) {
            return Ok(());
        }
        tracing::info!("PageRank cache empty — computing lazily (single-flight)");
        if let Err(e) = self.compute_pagerank_warm_locked(0.85, 20, &GraphScope::code_only(), None)
        {
            tracing::warn!("lazy PageRank computation failed: {e}");
        }
        Ok(())
    }

    /// Pre-build the PPR adjacency cache for the unified graph scope.
    ///
    /// Calling this at daemon startup eliminates the ~350ms first-query
    /// latency caused by building the adjacency list from the database on
    /// the first PPR request. The result is stored in `ppr_graph_cache`
    /// and will be reused by subsequent `personalized_pagerank*` calls
    /// until the graph generation changes.
    pub fn warm_ppr_cache(&self) -> Result<(), StoreError> {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_index_publication_dirty() {
            self.invalidate_ranking_caches_locked();
            return Err(StoreError::RankingUnavailable);
        }
        let scope = GraphScope::unified();
        self.load_ppr_graph(&scope, None)?;
        Ok(())
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

    use super::GraphScope;
    use crate::db::GraphStore;
    use crate::error::StoreError;

    fn make_symbol(uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo-1".to_string(),
            file_path: "src/lib.rs".to_string(),
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

    fn make_calls_edge(src: &str, tgt: &str) -> ResolvedEdge {
        ResolvedEdge {
            source_uid: src.to_string(),
            target_uid: tgt.to_string(),
            edge_type: EdgeType::Calls,
            confidence: 1.0,
            link_type: None,
            evidence: Vec::new(),
        }
    }

    fn test_store() -> GraphStore {
        GraphStore::in_memory().unwrap()
    }

    /// Seed a store with enough symbols/edges that a full `compute_pagerank`
    /// takes long enough for concurrent callers to genuinely race (nw-029).
    fn seed_small_store() -> GraphStore {
        let store = test_store();
        let n = 400usize;
        for i in 0..n {
            store
                .insert_symbol(&make_symbol(&format!("S{i}"), &format!("fn_{i}")))
                .unwrap();
        }
        // Each node calls the next three (wrapping) — a dense-enough graph
        // that the 20-iteration power method plus the DB load per compute
        // gives a wide race window.
        for i in 0..n {
            for d in 1..=3usize {
                let tgt = (i + d) % n;
                store
                    .insert_edge(&make_calls_edge(&format!("S{i}"), &format!("S{tgt}")))
                    .unwrap();
            }
        }
        store
    }

    #[test]
    fn pagerank_sidecar_binds_identity_generation_algorithm_and_payload() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.lbug");
        let sidecar = dir.path().join("graph.lbug.pagerank.json");
        let store = GraphStore::create(&db).unwrap();
        store.insert_symbol(&make_symbol("A", "alpha")).unwrap();
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        store.save_pagerank_cache(&sidecar).unwrap();

        let envelope: crate::artifact_envelope::ArtifactEnvelope =
            serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
        let identity = store.publication_identity().unwrap().unwrap();
        assert_eq!(envelope.brain_uuid, identity.brain_uuid);
        assert_eq!(envelope.publication_uuid, identity.publication_uuid);
        assert_eq!(envelope.source_graph_generation, store.graph_generation());
        assert_eq!(
            envelope.artifact_schema_version,
            super::PAGERANK_ARTIFACT_SCHEMA_VERSION
        );
        assert!(
            envelope
                .algorithm_fingerprint
                .starts_with(super::PAGERANK_ALGORITHM_FINGERPRINT_PREFIX)
        );
        drop(store);

        let reopened = GraphStore::open(&db).unwrap();
        reopened.load_pagerank_cache(&sidecar).unwrap();
        assert_eq!(reopened.pagerank_scores().unwrap().get("A"), Some(&1.0));
    }

    #[test]
    fn pagerank_sidecar_rejects_legacy_foreign_stale_and_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let source_db = dir.path().join("source.lbug");
        let source_sidecar = dir.path().join("source.lbug.pagerank.json");
        let source = GraphStore::create(&source_db).unwrap();
        source.insert_symbol(&make_symbol("A", "alpha")).unwrap();
        source
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        source.save_pagerank_cache(&source_sidecar).unwrap();
        let valid_bytes = std::fs::read(&source_sidecar).unwrap();

        std::fs::write(&source_sidecar, br#"{"legacy":1.0}"#).unwrap();
        let legacy = source.load_pagerank_cache(&source_sidecar).unwrap_err();
        assert!(
            legacy.to_string().contains("run a full reindex"),
            "{legacy}"
        );
        assert!(source.pagerank_cache.lock().unwrap().is_none());

        std::fs::write(&source_sidecar, &valid_bytes).unwrap();
        source.bump_graph_generation();
        let stale = source.load_pagerank_cache(&source_sidecar).unwrap_err();
        assert!(
            stale.to_string().contains("stale artifact generation"),
            "{stale}"
        );
        assert!(source.pagerank_cache.lock().unwrap().is_none());

        let foreign_db = dir.path().join("foreign.lbug");
        let foreign = GraphStore::create(&foreign_db).unwrap();
        let foreign_sidecar = dir.path().join("foreign.lbug.pagerank.json");
        std::fs::write(&foreign_sidecar, &valid_bytes).unwrap();
        let foreign_error = foreign.load_pagerank_cache(&foreign_sidecar).unwrap_err();
        assert!(
            foreign_error
                .to_string()
                .contains("foreign artifact identity"),
            "{foreign_error}"
        );
        assert!(foreign.pagerank_cache.lock().unwrap().is_none());

        let mut corrupt: crate::artifact_envelope::ArtifactEnvelope =
            serde_json::from_slice(&valid_bytes).unwrap();
        corrupt.payload = serde_json::json!({"A": 0.5});
        std::fs::write(
            &foreign_sidecar,
            serde_json::to_vec_pretty(&corrupt).unwrap(),
        )
        .unwrap();
        // Rebind only the identity so checksum validation, rather than the
        // earlier foreign-identity gate, diagnoses the payload mutation.
        let foreign_identity = foreign.publication_identity().unwrap().unwrap();
        corrupt.brain_uuid = foreign_identity.brain_uuid;
        corrupt.publication_uuid = foreign_identity.publication_uuid;
        std::fs::write(
            &foreign_sidecar,
            serde_json::to_vec_pretty(&corrupt).unwrap(),
        )
        .unwrap();
        let corrupt_error = foreign.load_pagerank_cache(&foreign_sidecar).unwrap_err();
        assert!(
            corrupt_error
                .to_string()
                .contains("corrupt artifact payload checksum"),
            "{corrupt_error}"
        );
        assert!(foreign.pagerank_cache.lock().unwrap().is_none());
    }

    #[test]
    fn ensure_pagerank_loaded_is_single_flight() {
        // nw-029: N concurrent callers observing an empty cache must produce
        // exactly ONE compute. compute_pagerank_warm bumps pagerank_generation
        // once per completed compute, so generation is the duplicate counter.
        let store = std::sync::Arc::new(seed_small_store());
        let initial = store.pagerank_generation();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = store.clone();
            let b = barrier.clone();
            handles.push(std::thread::spawn(move || {
                b.wait();
                s.ensure_pagerank_loaded().unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            store.pagerank_generation(),
            initial + 1,
            "8 concurrent callers must trigger exactly one compute"
        );
    }

    #[test]
    fn dirty_publication_blocks_pagerank_load_compute_save_and_serving() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = std::path::PathBuf::from(format!("{}.index-dirty", db_path.display()));
        let pagerank_path =
            std::path::PathBuf::from(format!("{}.pagerank.json", db_path.display()));
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.insert_symbol(&make_symbol("A", "fn_a")).unwrap();
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        assert!(store.pagerank_scores().unwrap().contains_key("A"));
        let clean_pagerank_generation = store.pagerank_generation();

        let publication = store.acquire_index_publication_lease().unwrap();
        let dirty_generation = store
            .with_index_publication_rank_barrier(|| -> Result<u64, StoreError> {
                std::fs::write(&marker_path, b"dirty")
                    .map_err(|error| StoreError::Query(error.to_string()))?;
                publication.reserve_generation()
            })
            .unwrap();

        // The module-header contract: every ranking QUERY path fails closed
        // during the window — none may answer with a successful-looking
        // empty result that a caller could mistake for an empty graph.
        assert!(
            matches!(store.pagerank_scores(), Err(StoreError::RankingUnavailable)),
            "dirty publication must not serve ranks computed while clean"
        );
        assert!(
            store.symbols_by_pagerank(None).is_err(),
            "dirty publication must not answer a ranked list query as an empty list"
        );
        assert!(
            store.ensure_pagerank_loaded().is_err(),
            "dirty publication must refuse the lazy load/compute"
        );
        assert!(
            store
                .compute_pagerank(0.85, 20, &GraphScope::code_only())
                .is_err(),
            "explicit PageRank computation must fail closed while dirty"
        );
        assert!(
            store
                .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::code_only())
                .is_err(),
            "PPR queries must fail closed while dirty"
        );
        assert!(
            store.warm_ppr_cache().is_err(),
            "PPR cache warming must fail closed while dirty"
        );
        assert_eq!(
            store.pagerank_generation(),
            clean_pagerank_generation,
            "dirty lazy access must not compute PageRank"
        );

        // The sidecar pair stays a no-op: refusing to persist or load during
        // the window is not a degraded answer.
        store.save_pagerank_cache(&pagerank_path).unwrap();
        assert!(
            !pagerank_path.exists(),
            "dirty PageRank state must never be persisted"
        );
        store.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(
            store
                .pagerank_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_none(),
            "dirty sidecar load must not populate the cache"
        );
        assert_eq!(store.pagerank_generation(), clean_pagerank_generation);

        let clean_generation = publication.clean_generation().unwrap();
        assert!(clean_generation > dirty_generation);
        store
            .with_index_publication_rank_barrier(|| -> Result<(), StoreError> {
                publication.publish_clean_generation()?;
                std::fs::remove_file(&marker_path)
                    .map_err(|error| StoreError::Query(error.to_string()))?;
                publication.complete_generation()?;
                Ok(())
            })
            .unwrap();
        publication.release().unwrap();
        assert!(
            store
                .pagerank_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_none(),
            "dirty-window ranks must not survive clean publication"
        );
        assert_eq!(store.pagerank_generation(), clean_pagerank_generation);
        assert!(store.pagerank_scores().unwrap().contains_key("A"));
        assert_eq!(
            store.pagerank_generation(),
            clean_pagerank_generation + 1,
            "the first clean access must perform the only retained recompute"
        );
    }

    #[test]
    fn invalidate_pagerank_recomputes_after_code_deletion() {
        // nw-055 (P1b): a code-repo deletion changes the SURVIVING nodes' ranks,
        // but the `pagerank_cache` score map is NOT generation-keyed — a rank
        // query after the delete would keep serving scores computed over the
        // PRE-deletion graph. `invalidate_pagerank` drops the cache so the next
        // rank query recomputes over the reduced graph via the nw-029
        // single-flight. RED pre-fix (no invalidate): stale scores still list
        // the removed repo's symbols and no recompute happens.
        let store = test_store();

        // repo-1 survivors: A -> B -> C. repo-2: D, E (deleted below).
        for uid in ["A", "B", "C"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        for uid in ["D", "E"] {
            let mut s = make_symbol(uid, &format!("fn_{uid}"));
            s.repo_uid = "repo-2".to_string();
            s.file_path = "src/other.rs".to_string();
            store.insert_symbol(&s).unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("D", "C")).unwrap();
        store.insert_edge(&make_calls_edge("E", "C")).unwrap();

        // Warm the cache and capture the pre-deletion state.
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        let gen_before = store.pagerank_generation();
        let scores_before = store.pagerank_scores().unwrap();
        assert!(
            scores_before.contains_key("D") && scores_before.contains_key("E"),
            "repo-2 symbols should be ranked before the deletion"
        );

        // Remove repo-2 from the store, then invalidate the score cache (this
        // mirrors what remove_repo / prune_stale now do in the daemon).
        store.bulk_delete_repo_files_and_symbols("repo-2").unwrap();
        store.invalidate_pagerank();

        // The cache is now empty — the next rank query must recompute.
        assert!(
            store.pagerank_cache.lock().unwrap().is_none(),
            "invalidate_pagerank must clear the in-memory score cache"
        );

        // A rank query triggers the single-flight recompute over the reduced graph.
        let scores_after = store.pagerank_scores().unwrap();
        let gen_after = store.pagerank_generation();

        // Primary assertion: a recompute happened (invalidation fired).
        assert!(
            gen_after > gen_before,
            "rank query after invalidate must recompute PageRank (gen {gen_before} -> {gen_after})"
        );
        // Behavioral assertion: recomputed scores reflect repo-2's absence.
        assert!(
            !scores_after.contains_key("D") && !scores_after.contains_key("E"),
            "recomputed ranks must not include the deleted repo-2 symbols"
        );
        assert!(
            scores_after.contains_key("A")
                && scores_after.contains_key("B")
                && scores_after.contains_key("C"),
            "surviving repo-1 symbols must still be ranked"
        );
    }

    #[test]
    fn invalidate_pagerank_waits_for_in_flight_lazy_compute() {
        let store = std::sync::Arc::new(test_store());
        let (compute_started_tx, compute_started_rx) = std::sync::mpsc::channel();
        let (release_compute_tx, release_compute_rx) = std::sync::mpsc::channel();

        let computing_store = store.clone();
        let compute = std::thread::spawn(move || {
            let _flight = computing_store.pagerank_compute_lock.lock().unwrap();
            compute_started_tx.send(()).unwrap();
            release_compute_rx.recv().unwrap();
        });
        compute_started_rx.recv().unwrap();

        let (invalidate_started_tx, invalidate_started_rx) = std::sync::mpsc::channel();
        let (invalidate_done_tx, invalidate_done_rx) = std::sync::mpsc::channel();
        let invalidating_store = store.clone();
        let invalidation = std::thread::spawn(move || {
            invalidate_started_tx.send(()).unwrap();
            invalidating_store.invalidate_pagerank();
            invalidate_done_tx.send(()).unwrap();
        });
        invalidate_started_rx.recv().unwrap();

        assert!(
            invalidate_done_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "invalidation must not complete while a lazy PageRank compute can still publish scores"
        );

        release_compute_tx.send(()).unwrap();
        compute.join().unwrap();
        invalidate_done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("invalidation should complete after the compute flight is released");
        invalidation.join().unwrap();
    }

    #[test]
    fn compute_pagerank_clears_scores_when_graph_becomes_empty() {
        let store = test_store();
        store.insert_symbol(&make_symbol("A", "fn_a")).unwrap();
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        assert!(store.pagerank_scores().unwrap().contains_key("A"));

        assert_eq!(
            store
                .delete_symbols_in_file("repo-1", "src/lib.rs")
                .unwrap(),
            1
        );
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();

        assert!(
            store.pagerank_scores().unwrap().is_empty(),
            "an empty graph must replace previously cached scores with an empty cache"
        );
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

    /// nw-061, corrected: a 2-node graph scoring 0.5/0.5 is NOT edge-blindness.
    ///
    /// The item reported `{a: 0.5, b: 0.5}` for a single `b -> a` edge and
    /// concluded "the ranking loader's edge queries see no rows". Measured, the
    /// loader returns `incoming=[[(1, 1.0)], [(0, 0.3)]]` with
    /// `out_weight=[0.3, 1.0]` — the forward CALLS edge at 1.0 AND a reverse
    /// edge at 0.3. Both are present.
    ///
    /// PageRank normalises each contribution by the source's total out-weight,
    /// so `a`'s single 0.3 out-edge divided by `out_weight[a]` of 0.3 is 1.0 —
    /// exactly what `b`'s 1.0/1.0 gives. On a TWO-node graph that makes the
    /// system perfectly symmetric, and 0.5/0.5 is the correct fixed point. The
    /// asymmetry only survives when a node's out-weight differs from the weight
    /// of the edge being followed, which needs a third node.
    #[test]
    fn two_node_graph_is_symmetric_after_out_degree_normalisation() {
        let store = test_store();
        store.insert_symbol(&make_symbol("a", "fn_a")).unwrap();
        store.insert_symbol(&make_symbol("b", "fn_b")).unwrap();
        store.insert_edge(&make_calls_edge("b", "a")).unwrap();

        let (uids, _idx, incoming, out_weight) = store
            .load_ppr_graph(&GraphScope::code_only(), None)
            .unwrap();
        assert_eq!(uids.len(), 2);
        assert!(
            incoming.iter().all(|v| !v.is_empty()),
            "both directions must be loaded — an empty side WOULD be the \
             edge-blindness nw-061 suspected: {incoming:?}"
        );
        assert!(
            out_weight.iter().all(|&w| w > 0.0),
            "neither node is dangling once the reverse edge exists: {out_weight:?}"
        );
    }

    /// The real guard against edge-blindness, and the one nw-061 asked for.
    ///
    /// If the ranking loader ever stopped seeing edges, every node would be
    /// dangling, scores would go uniform, and this assertion fails. It needs
    /// three or more nodes precisely because of the two-node symmetry above.
    #[test]
    fn pagerank_is_edge_sensitive_hub_outranks_leaf() {
        let store = test_store();
        for uid in ["hub", "leaf", "x", "y"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("x", "hub")).unwrap();
        store.insert_edge(&make_calls_edge("y", "hub")).unwrap();
        store.insert_edge(&make_calls_edge("leaf", "hub")).unwrap();

        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        let ranked = store.symbols_by_pagerank(None).unwrap();
        let score_of = |uid: &str| -> f64 {
            ranked
                .iter()
                .find(|s| s.uid == uid)
                .and_then(|s| s.pagerank_score)
                .unwrap_or(0.0)
        };
        let (hub, leaf) = (score_of("hub"), score_of("leaf"));
        assert!(
            hub > leaf,
            "a node with three inbound calls must outrank one with none; \
             equal scores mean ranking has gone edge-blind (hub={hub:.4} leaf={leaf:.4})"
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
                    embedding: None,
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
                    embedding: None,
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
                    embedding: None,
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
                ("sec:note:a", "note:b", 1.0, "B", "B"),
                ("sec:note:c", "note:b", 1.0, "B", "B"),
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
                    embedding: None,
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
                evidence: Vec::new(),
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
    fn ppr_interaction_bias_boosts_accessed_nodes() {
        // Graph: A->B->C->D
        // Seed from A. Without interaction bias, B ranks highest after A
        // (directly called). With interaction bias on D, D should get a
        // ranking boost compared to the baseline.
        use std::collections::HashMap;

        let store = test_store();
        for uid in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("C", "D")).unwrap();

        // Baseline: no interaction cache.
        let baseline = store
            .personalized_pagerank(&["A".to_string()], 0.85, 40, &GraphScope::code_only())
            .unwrap();
        let baseline_d = baseline
            .iter()
            .find(|(u, _)| u == "D")
            .map(|(_, s)| *s)
            .unwrap_or(0.0);

        // Load interaction scores that heavily favor D.
        let mut scores = HashMap::new();
        scores.insert("D".to_string(), 10.0);
        store.load_interaction_cache(scores);

        let biased = store
            .personalized_pagerank(&["A".to_string()], 0.85, 40, &GraphScope::code_only())
            .unwrap();
        let biased_d = biased
            .iter()
            .find(|(u, _)| u == "D")
            .map(|(_, s)| *s)
            .unwrap_or(0.0);

        assert!(
            biased_d > baseline_d,
            "interaction bias should boost D: biased={biased_d:.6} vs baseline={baseline_d:.6}"
        );
    }

    #[test]
    fn ppr_empty_interaction_cache_matches_baseline() {
        // When the interaction cache is loaded but empty, PPR should produce
        // identical results to having no cache at all.
        use std::collections::HashMap;

        let store = test_store();
        for uid in ["A", "B", "C"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();

        // Baseline with no interaction cache.
        let baseline = store
            .personalized_pagerank(&["A".to_string()], 0.85, 40, &GraphScope::code_only())
            .unwrap();

        // Load an empty interaction cache.
        store.load_interaction_cache(HashMap::new());

        let with_empty = store
            .personalized_pagerank(&["A".to_string()], 0.85, 40, &GraphScope::code_only())
            .unwrap();

        assert_eq!(
            baseline.len(),
            with_empty.len(),
            "empty interaction cache should not change result count"
        );
        for ((uid1, s1), (uid2, s2)) in baseline.iter().zip(with_empty.iter()) {
            assert_eq!(uid1, uid2);
            assert!(
                (s1 - s2).abs() < 1e-10,
                "scores should be identical with empty cache: {uid1}: {s1} vs {s2}"
            );
        }
    }

    #[test]
    fn interaction_cache_load_and_clear() {
        use std::collections::HashMap;

        let store = test_store();
        let mut scores = HashMap::new();
        scores.insert("A".to_string(), 1.0);
        store.load_interaction_cache(scores);

        // Verify it's loaded.
        let cache = store.interaction_cache.lock().unwrap();
        assert!(cache.is_some());
        drop(cache);

        // Clear it.
        store.clear_interaction_cache();
        let cache = store.interaction_cache.lock().unwrap();
        assert!(cache.is_none());
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
            embedding: None,
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
            embedding: None,
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
            embedding: None,
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
            embedding: None,
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
                embedding: None,
            };
            store.upsert_note(&filler).unwrap();

            let section = Section {
                uid: sec_uid.clone(),
                note_uid: filler_uid.clone(),
                heading_uid: None,
                start_line: 1,
                end_line: 5,
                text_hash: format!("th{i}"),
                text_content: "see [[Popular X]] and [[Popular Y]]".to_string(),
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
                    (&sec_uid, "note:popular_x", 1.0, "Popular X", "Popular X"),
                    (&sec_uid, "note:popular_y", 1.0, "Popular Y", "Popular Y"),
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

    // ── PPR graph cache ──────────────────────────────────────────────────

    #[test]
    fn ppr_graph_cache_hit_produces_identical_results() {
        // Running PPR twice with the same scope + intent must produce byte-for-byte
        // identical results; the second call should hit the cache.
        let store = test_store();
        for uid in ["A", "B", "C", "D"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "C")).unwrap();
        store.insert_edge(&make_calls_edge("D", "C")).unwrap();

        let first = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::code_only())
            .unwrap();
        let second = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::code_only())
            .unwrap();

        assert_eq!(
            first.len(),
            second.len(),
            "cached PPR must produce the same number of results"
        );
        for ((uid1, s1), (uid2, s2)) in first.iter().zip(second.iter()) {
            assert_eq!(uid1, uid2, "cached PPR must return same UIDs in same order");
            assert!(
                (s1 - s2).abs() < 1e-15,
                "cached PPR scores must be identical: {uid1}: {s1} vs {s2}"
            );
        }
    }

    #[test]
    fn ppr_graph_cache_invalidates_on_scope_change() {
        // Different scopes must not share the same cache entry.
        let store = test_store();
        for uid in ["A", "B"] {
            store
                .insert_symbol(&make_symbol(uid, &format!("fn_{uid}")))
                .unwrap();
        }
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();

        // First call with code_only populates the cache.
        let code_results = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::code_only())
            .unwrap();

        // Second call with unified uses a different scope — must not return the
        // code_only cached graph (unified has more node types).
        let unified_results = store
            .personalized_pagerank(&["A".to_string()], 0.85, 20, &GraphScope::unified())
            .unwrap();

        // Both should contain A (the seed), but they used different graphs.
        assert!(
            code_results.iter().any(|(u, _)| u == "A"),
            "code_only results must include seed A"
        );
        assert!(
            unified_results.iter().any(|(u, _)| u == "A"),
            "unified results must include seed A"
        );
    }

    // ── Feature F12: git-activity rank-read multiplier ───────────────────

    #[test]
    fn git_activity_multiplier_neutral_when_no_score() {
        use super::git_activity_multiplier;
        let m = git_activity_multiplier(None, super::DEFAULT_GIT_ACTIVITY_WEIGHT);
        assert!((m - 1.0).abs() < f64::EPSILON, "absent score → neutral 1.0");
    }

    #[test]
    fn git_activity_multiplier_within_clamp() {
        use super::{GIT_ACTIVITY_MULT_MAX, GIT_ACTIVITY_MULT_MIN, git_activity_multiplier};
        let w = super::DEFAULT_GIT_ACTIVITY_WEIGHT;
        assert!((git_activity_multiplier(Some(0.5), w) - 1.0).abs() < 1e-9);
        assert!((git_activity_multiplier(Some(1.0), w) - 1.6).abs() < 1e-9);
        assert!((git_activity_multiplier(Some(0.0), w) - 0.4).abs() < 1e-9);
        for i in 0..=100 {
            let s = i as f64 / 100.0;
            let m = git_activity_multiplier(Some(s), 5.0);
            assert!((GIT_ACTIVITY_MULT_MIN..=GIT_ACTIVITY_MULT_MAX).contains(&m));
        }
    }

    #[test]
    fn git_activity_demotes_stale_file_at_read_time() {
        // Two symbols with identical structural position but different files.
        // Both get the same base pagerank; loading git-activity scores that
        // mark `fresh.rs` live (0.95) and `stale.rs` dormant (0.05) should
        // make the fresh symbol outrank the stale one at read time, without
        // touching the PPR fixpoint.
        let store = test_store();
        let mut fresh = make_symbol("F", "shared_name");
        fresh.file_path = "src/fresh.rs".to_string();
        let mut stale = make_symbol("S", "shared_name");
        stale.file_path = "src/stale.rs".to_string();
        store.insert_symbol(&fresh).unwrap();
        store.insert_symbol(&stale).unwrap();
        // Symmetric edges so PPR gives both the same base score.
        store.insert_edge(&make_calls_edge("F", "S")).unwrap();
        store.insert_edge(&make_calls_edge("S", "F")).unwrap();

        store
            .compute_pagerank(0.85, 30, &GraphScope::code_only())
            .unwrap();

        // Baseline (no git-activity loaded): scores should be ~equal.
        let baseline = store.symbols_by_pagerank(None).unwrap();
        let base_f = baseline.iter().find(|s| s.uid == "F").unwrap();
        let base_s = baseline.iter().find(|s| s.uid == "S").unwrap();
        assert!(
            (base_f.pagerank_score.unwrap() - base_s.pagerank_score.unwrap()).abs() < 1e-9,
            "symmetric graph → equal base pagerank"
        );

        // Load recency scores and re-read.
        let mut ga = std::collections::HashMap::new();
        ga.insert("src/fresh.rs".to_string(), 0.95);
        ga.insert("src/stale.rs".to_string(), 0.05);
        store.load_git_activity_cache(ga);

        let ranked = store.symbols_by_pagerank(None).unwrap();
        let f = ranked.iter().find(|s| s.uid == "F").unwrap();
        let s = ranked.iter().find(|s| s.uid == "S").unwrap();
        assert!(
            f.pagerank_score.unwrap() > s.pagerank_score.unwrap(),
            "actively-developed fresh.rs ({:.6}) should outrank dormant stale.rs ({:.6})",
            f.pagerank_score.unwrap(),
            s.pagerank_score.unwrap()
        );
        // Results stay sorted descending.
        assert_eq!(ranked[0].uid, "F");
    }

    #[test]
    fn git_activity_neutral_for_files_without_score() {
        // A symbol whose file has no recency score must keep its base pagerank
        // (multiplier 1.0) even when the cache is loaded for other files.
        let store = test_store();
        let mut a = make_symbol("A", "fn_a");
        a.file_path = "src/scored.rs".to_string();
        let mut b = make_symbol("B", "fn_b");
        b.file_path = "src/unscored.rs".to_string();
        store.insert_symbol(&a).unwrap();
        store.insert_symbol(&b).unwrap();
        store.insert_edge(&make_calls_edge("A", "B")).unwrap();
        store.insert_edge(&make_calls_edge("B", "A")).unwrap();
        store
            .compute_pagerank(0.85, 30, &GraphScope::code_only())
            .unwrap();

        let base_b = store
            .symbols_by_pagerank(None)
            .unwrap()
            .into_iter()
            .find(|s| s.uid == "B")
            .unwrap()
            .pagerank_score
            .unwrap();

        let mut ga = std::collections::HashMap::new();
        ga.insert("src/scored.rs".to_string(), 0.9); // only A's file
        store.load_git_activity_cache(ga);

        let after_b = store
            .symbols_by_pagerank(None)
            .unwrap()
            .into_iter()
            .find(|s| s.uid == "B")
            .unwrap()
            .pagerank_score
            .unwrap();
        assert!(
            (base_b - after_b).abs() < 1e-9,
            "unscored file should keep base pagerank (neutral): {base_b} vs {after_b}"
        );
    }
}
