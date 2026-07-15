use std::collections::{HashMap, VecDeque};

use nestweaver_schema::{EdgeType, SymbolKind};

use crate::db::GraphStore;
use crate::error::StoreError;
use crate::ranking::{PathDeboostRule, SeedResolutionConfig};

/// Compute the multiplicative path-factor for a symbol's `file_path` against
/// a list of [`PathDeboostRule`]s. Matching rules multiply; factor=1.0 when
/// no rule matches.
///
/// Prefixes are matched case-insensitively against the (`/`-prepended,
/// lowercased) haystack so patterns like `/playwright/`, `/cypress/`, and
/// `/__tests__/` anchor on the first directory segment of a repo-relative
/// path (`playwright/components/Foo.tsx` → matched; `myplaywright/...` → not).
///
/// Suffixes are matched case-sensitively against the raw `file_path`
/// (suffix rules like `.test.ts` rely on conventional extensions).
fn compute_path_factor(file_path: &str, rules: &[PathDeboostRule]) -> f64 {
    let mut prepended = String::with_capacity(file_path.len() + 1);
    prepended.push('/');
    prepended.push_str(&file_path.to_lowercase());
    let mut factor = 1.0_f64;
    for rule in rules {
        let matched = match (&rule.prefix, &rule.suffix) {
            (Some(prefix), None) => prepended.contains(prefix.as_str()),
            (None, Some(suffix)) => file_path.ends_with(suffix.as_str()),
            // Invalid rule shapes are rejected at config-load time, so by
            // the time we get here a rule is guaranteed to have exactly one
            // of {prefix, suffix} set. Be tolerant in the store layer just
            // in case (skip the rule).
            _ => false,
        };
        if matched {
            factor *= rule.factor;
        }
    }
    factor
}

/// Index of `kind` in the user-defined `kind_priority` list (lower = higher
/// priority). Returns `usize::MAX` for kinds not present in the list, which
/// effectively pushes them to the bottom of any kind-based tiebreak.
fn kind_rank(kind: SymbolKind, kind_priority: &[String]) -> usize {
    let name = kind.as_str();
    kind_priority
        .iter()
        .position(|k| k == name)
        .unwrap_or(usize::MAX)
}

/// Cached result of a full symbol table scan, keyed on `graph_generation`.
///
/// Stored in `GraphStore::symbol_name_cache` to avoid repeated full-table
/// scans in `search_symbols_by_name`. Valid as long as `generation` matches
/// the current `graph_generation`; stale once any reindex bumps that counter.
pub(crate) struct SymbolNameCached {
    /// The `graph_generation` value at cache-fill time.
    pub generation: u64,
    /// All symbols together with their pre-lowercased names for O(n) contains
    /// matching without re-allocating on every call.
    pub symbols: Vec<(String, nestweaver_schema::Symbol)>,
}

/// Minimum impact score for a node to be included in traversal results.
/// Edges below this threshold are pruned during BFS.
const DEFAULT_IMPACT_THRESHOLD: f64 = 0.10;

/// A node returned by the impact analysis traversal.
#[derive(Debug, Clone)]
pub struct ImpactNode {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub start_line: u32,
    pub edge_type: String,
    pub confidence: f32,
    pub depth: u32,
    /// Confidence-weighted impact score. The changed symbol starts at 1.0;
    /// each traversal step multiplies by the edge confidence. Ranges 0.0–1.0.
    pub impact_score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImpactEdge {
    pub target_uid: String,
    pub edge_type: String,
    pub confidence: f32,
}

/// Result of an impact traversal plus honesty flags about whether the walk
/// was complete. Truncation means real dependents may exist beyond `nodes`.
#[derive(Debug, Clone)]
pub struct ImpactResult {
    pub nodes: Vec<ImpactNode>,
    /// A path was pruned because its decayed score fell below the impact
    /// threshold — the tail of the impact set may be incomplete.
    pub truncated_by_threshold: bool,
    /// A frontier node was reached at `max_depth` and left unexpanded —
    /// deeper dependents may exist beyond the returned set.
    pub truncated_by_depth: bool,
    /// The edge types actually traversed.
    pub edge_types: Vec<EdgeType>,
}

/// A row representing caller + edge metadata returned from the BFS query.
struct CallerRow {
    uid: String,
    name: String,
    file_path: String,
    start_line: u32,
    edge_type: String,
    confidence: f32,
}

const IMPACT_EDGE_TYPES: &[EdgeType] = &[
    EdgeType::Calls,
    EdgeType::Imports,
    EdgeType::Extends,
    EdgeType::Implements,
    EdgeType::Includes,
    EdgeType::CrossRepoLink,
];

impl GraphStore {
    /// Find all symbols that directly or transitively call/import/extend/implement `target_uid`.
    ///
    /// Performs confidence-weighted BFS up to `max_depth` levels following incoming
    /// edges of type CALLS, IMPORTS, EXTENDS_SYM, IMPLEMENTS_SYM, and INCLUDES_SYM.
    ///
    /// Each node receives an `impact_score` that starts at 1.0 for the seed and
    /// decays multiplicatively through each edge's confidence value. Traversal is
    /// pruned when a node's score falls below `DEFAULT_IMPACT_THRESHOLD` (0.10).
    /// The `min_confidence` parameter provides an additional per-edge filter.
    ///
    /// Results are sorted by `impact_score` descending (highest impact first).
    pub fn impact(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
    ) -> Result<Vec<ImpactNode>, StoreError> {
        self.impact_cancellable(target_uid, max_depth, min_confidence, None)
    }

    /// Like [`impact`](Self::impact), but cooperatively bails when `cancel`
    /// trips (a query timeout or client disconnect). The flag is checked once
    /// per BFS dequeue; once tripped the walk returns
    /// `Err(StoreError::Cancelled(_))` — a cancelled traversal is *incomplete*,
    /// distinct from a legitimately empty result, so no caller mistakes the
    /// truncated walk for a real answer (or caches it). `cancel = None` never
    /// trips and is byte-for-byte the original behavior.
    pub fn impact_cancellable(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Vec<ImpactNode>, StoreError> {
        Ok(self
            .impact_detailed(
                target_uid,
                max_depth,
                min_confidence,
                IMPACT_EDGE_TYPES,
                cancel,
            )?
            .nodes)
    }

    /// Impact with the default edge set, returning the ImpactResult (with
    /// truncation-honesty flags). Convenience over `impact_detailed`.
    pub fn impact_with_flags(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
    ) -> Result<ImpactResult, StoreError> {
        self.impact_detailed(target_uid, max_depth, min_confidence, IMPACT_EDGE_TYPES, None)
    }

    /// Confidence-weighted reverse BFS that also reports whether the walk was
    /// complete. `edges` selects which incoming relationship types to follow;
    /// pass [`IMPACT_EDGE_TYPES`] for the default impact edge set. The returned
    /// [`ImpactResult`] carries the ranked nodes plus `truncated_by_threshold`
    /// / `truncated_by_depth` honesty flags so callers can tell an *incomplete*
    /// walk from a genuinely small impact set.
    ///
    /// `cancel` behaves exactly as in [`impact_cancellable`](Self::impact_cancellable):
    /// the flag is checked once per dequeue and a tripped flag returns
    /// `Err(StoreError::Cancelled(_))`.
    pub fn impact_detailed(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        edges: &[EdgeType],
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        // Track the best impact score seen so far for each node.
        let mut scores: HashMap<String, f64> = HashMap::new();
        scores.insert(target_uid.to_string(), 1.0);

        // Queue entries: (uid, depth)
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        queue.push_back((target_uid.to_string(), 0));

        // Store result nodes keyed by uid so we can update scores if a
        // better path is found.
        let mut result_map: HashMap<String, ImpactNode> = HashMap::new();

        // Honesty flags: whether the walk left part of the impact set unseen.
        let mut truncated_by_threshold = false;
        let mut truncated_by_depth = false;

        while let Some((current_uid, depth)) = queue.pop_front() {
            if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
                // The shared cancel flag is a bare bool and can't carry a
                // reason, so the leaf always reports `Timeout` (see
                // `CancelReason`).
                return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
            }
            if depth >= max_depth {
                // A frontier node reached the depth boundary unexpanded;
                // deeper dependents may exist beyond the returned set.
                truncated_by_depth = true;
                continue;
            }

            let parent_score = scores.get(&current_uid).copied().unwrap_or(0.0);

            let callers = self.direct_callers_of(&current_uid, min_confidence, edges)?;

            for row in callers {
                // Skip the seed node itself.
                if row.uid == target_uid {
                    continue;
                }

                let candidate_score = parent_score * row.confidence as f64;

                // Prune paths that fall below the impact threshold.
                if candidate_score < DEFAULT_IMPACT_THRESHOLD {
                    truncated_by_threshold = true;
                    continue;
                }

                let prev_score = scores.get(&row.uid).copied().unwrap_or(0.0);

                if candidate_score > prev_score {
                    scores.insert(row.uid.clone(), candidate_score);

                    let node = ImpactNode {
                        uid: row.uid.clone(),
                        name: row.name,
                        file_path: row.file_path,
                        start_line: row.start_line,
                        edge_type: row.edge_type,
                        confidence: row.confidence,
                        depth: depth + 1,
                        impact_score: candidate_score,
                    };
                    result_map.insert(row.uid.clone(), node);

                    // Re-enqueue so downstream nodes can pick up the
                    // improved score. This is safe because scores only
                    // increase (like Dijkstra with max instead of min).
                    queue.push_back((row.uid, depth + 1));
                }
            }
        }

        let mut results: Vec<ImpactNode> = result_map.into_values().collect();
        // Sort by impact_score descending; break ties by uid for determinism.
        results.sort_by(|a, b| {
            b.impact_score
                .partial_cmp(&a.impact_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.uid.cmp(&b.uid))
        });

        Ok(ImpactResult {
            nodes: results,
            truncated_by_threshold,
            truncated_by_depth,
            edge_types: edges.to_vec(),
        })
    }

    /// Internal: fetch all direct callers of `uid` across
    /// CALLS/IMPORTS/EXTENDS_SYM/IMPLEMENTS_SYM/INCLUDES_SYM/CROSS_REPO_LINK.
    fn direct_callers_of(
        &self,
        uid: &str,
        min_confidence: f32,
        edges: &[EdgeType],
    ) -> Result<Vec<CallerRow>, StoreError> {
        let conn = self.conn()?;
        let min_conf = min_confidence as f64;
        let mut rows: Vec<CallerRow> = Vec::new();

        for edge_type in edges.iter().map(|edge_type| edge_type.rel_table_name()) {
            let q = format!(
                "MATCH (s:Symbol)-[r:{et}]->(t:Symbol {{uid: $uid}}) \
                 WHERE r.confidence >= $min_conf \
                 RETURN s.uid, s.name, s.file_path, s.start_line, r.confidence",
                et = edge_type,
            );

            let mut stmt = match conn.prepare(&q) {
                Ok(s) => s,
                Err(e) => {
                    tracing::trace!(
                        "impact: edge type {edge_type} skipped (table may not exist): {e}"
                    );
                    continue;
                }
            };
            let result = match conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(uid.to_string())),
                    ("min_conf", lbug::Value::Double(min_conf)),
                ],
            ) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!("impact: edge type {edge_type} query failed: {e}");
                    continue;
                }
            };

            for row in result {
                use lbug::Value;
                let caller_uid = match &row[0] {
                    Value::String(s) => s.clone(),
                    _ => continue,
                };
                let name = match &row[1] {
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                let file_path = match &row[2] {
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                let start_line = match &row[3] {
                    Value::Int64(n) => u32::try_from(*n).unwrap_or(0),
                    Value::Int32(n) => u32::try_from(*n).unwrap_or(0),
                    _ => 0,
                };
                let confidence = match &row[4] {
                    Value::Float(f) => *f,
                    Value::Double(f) => *f as f32,
                    _ => 0.0,
                };

                rows.push(CallerRow {
                    uid: caller_uid,
                    name,
                    file_path,
                    start_line,
                    edge_type: edge_type.to_string(),
                    confidence,
                });
            }
        }

        Ok(rows)
    }

    /// Return outgoing edges that use the same relationship types as impact
    /// traversal.
    pub fn outgoing_impact_edges(
        &self,
        source_uid: &str,
        min_confidence: f32,
    ) -> Result<Vec<ImpactEdge>, StoreError> {
        let conn = self.conn()?;
        let min_conf = min_confidence as f64;
        let mut edges = Vec::new();

        for edge_type in IMPACT_EDGE_TYPES
            .iter()
            .map(|edge_type| edge_type.rel_table_name())
        {
            let q = format!(
                "MATCH (s:Symbol {{uid: $uid}})-[r:{et}]->(t:Symbol) \
                 WHERE r.confidence >= $min_conf \
                 RETURN t.uid, r.confidence",
                et = edge_type,
            );
            let mut stmt = match conn.prepare(&q) {
                Ok(s) => s,
                Err(e) => {
                    tracing::trace!(
                        "outgoing_impact_edges: edge type {edge_type} skipped (table may not exist): {e}"
                    );
                    continue;
                }
            };
            let result = match conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(source_uid.to_string())),
                    ("min_conf", lbug::Value::Double(min_conf)),
                ],
            ) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!(
                        "outgoing_impact_edges: edge type {edge_type} query failed: {e}"
                    );
                    continue;
                }
            };

            for row in result {
                use lbug::Value;
                let target_uid = match &row[0] {
                    Value::String(s) => s.clone(),
                    _ => continue,
                };
                let confidence = match &row[1] {
                    Value::Float(f) => *f,
                    Value::Double(f) => *f as f32,
                    _ => 0.0,
                };

                edges.push(ImpactEdge {
                    target_uid,
                    edge_type: edge_type.to_string(),
                    confidence,
                });
            }
        }

        Ok(edges)
    }

    /// Search symbols whose name contains `query` (case-insensitive substring match).
    /// Returns up to `limit` results.
    ///
    /// The full symbol table is loaded once per `graph_generation` and cached in
    /// `symbol_name_cache`. Subsequent calls within the same generation skip the
    /// DB round-trip entirely and filter the in-memory list.
    ///
    /// Candidates are scored by:
    /// 1. **Name quality** — exact (`4.0`), prefix (`2.0`), or contains (`1.0`).
    /// 2. **Path factor** — multiplicative `[seed_resolution].path_deboost`
    ///    rules (defaults target JS/TS test mirrors like `playwright/`,
    ///    `__tests__/`, `*.test.ts`).
    /// 3. **Kind priority** — ties broken by `[seed_resolution].kind_priority`
    ///    so `Class` outranks `Property` of the same lowercased name.
    /// 4. **File path** — final tiebreaker, lexicographic ascending, for
    ///    deterministic stability across calls.
    pub fn search_symbols_by_name(
        &self,
        query: &str,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
    ) -> Result<Vec<nestweaver_schema::Symbol>, StoreError> {
        let needle = query.to_lowercase();
        let cur_gen = self.graph_generation();

        // --- Step 1: check the cache (hold the lock only briefly) -----------
        // On a hit we clone the Arc (cheap ref-count bump) rather than the
        // entire symbol Vec, so the lock is released before any filtering work.
        let cached_symbols: Option<std::sync::Arc<SymbolNameCached>> = {
            let guard = self
                .symbol_name_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref c) = *guard {
                if c.generation == cur_gen {
                    Some(std::sync::Arc::clone(c))
                } else {
                    None
                }
            } else {
                None
            }
        };

        // --- Step 2: on cache miss, query the DB then populate the cache ----
        let entry: std::sync::Arc<SymbolNameCached> = if let Some(arc) = cached_symbols {
            arc
        } else {
            // LadybugDB's CONTAINS is case-sensitive and has no toLower().
            // Load all symbols and filter in Rust for case-insensitive matching.
            let conn = self.conn()?;
            let q = format!("MATCH (s:Symbol) RETURN {}", crate::read::SYMBOL_COLUMNS);
            let result = conn
                .query(&q)
                .map_err(|e| StoreError::Query(format!("query: {e}")))?;

            let mut all: Vec<(String, nestweaver_schema::Symbol)> = Vec::new();
            for row in result {
                if let Ok(sym) = crate::read::row_to_symbol(&row) {
                    let lower = sym.name.to_lowercase();
                    all.push((lower, sym));
                }
            }

            // Store the populated cache (re-check generation under the lock in
            // case another thread raced us, preferring the newer fill).
            let mut guard = self
                .symbol_name_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let current_gen = self.graph_generation();
            let should_update = match &*guard {
                None => true,
                Some(c) => c.generation != current_gen,
            };
            if should_update {
                let arc = std::sync::Arc::new(SymbolNameCached {
                    generation: current_gen,
                    symbols: all,
                });
                *guard = Some(std::sync::Arc::clone(&arc));
                arc
            } else {
                // Another thread raced us and filled the cache; use its entry.
                std::sync::Arc::clone(guard.as_ref().unwrap())
            }
        };

        // --- Step 3: filter and rank the in-memory list ----------------------
        // Collect all substring matches, score by name quality × path factor,
        // then take the top `limit` by descending adjusted score with
        // kind-priority + file-path tiebreaks. This prevents test/playwright
        // files from dominating when a PascalCase name also appears in
        // production code, and gives a deterministic order across calls.
        let mut candidates: Vec<(f64, &nestweaver_schema::Symbol)> = Vec::new();
        for (lower, sym) in &entry.symbols {
            if !lower.contains(&needle) {
                continue;
            }
            let base_score = if *lower == needle {
                4.0_f64 // exact
            } else if lower.starts_with(&needle) {
                2.0_f64 // prefix
            } else {
                1.0_f64 // contains
            };
            let path_factor = compute_path_factor(&sym.file_path, &seed_resolution.path_deboost);
            let adjusted = base_score * path_factor;
            candidates.push((adjusted, sym));
        }
        candidates.sort_by(|(a_score, a_sym), (b_score, b_sym)| {
            // 1) adjusted DESC
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // 2) kind_priority ASC (lower index = higher priority)
                .then_with(|| {
                    kind_rank(a_sym.kind, &seed_resolution.kind_priority)
                        .cmp(&kind_rank(b_sym.kind, &seed_resolution.kind_priority))
                })
                // 3) file_path lexicographic ASC for deterministic stability
                .then_with(|| a_sym.file_path.cmp(&b_sym.file_path))
        });
        Ok(candidates
            .into_iter()
            .take(limit)
            .map(|(_, sym)| sym.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use nestweaver_schema::{Symbol, SymbolKind, Visibility};

    use super::{compute_path_factor, kind_rank};
    use crate::db::GraphStore;
    use crate::ranking::{PathDeboostRule, SeedResolutionConfig, default_kind_priority};

    fn make_symbol(uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 10,
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

    /// Impact analysis must follow CROSS_REPO_LINK edges so that callers in
    /// other repos (modeled as cross-repo links in a unified multi-repo graph)
    /// are reported. Regression guard for cross-boundary intelligence: without
    /// CROSS_REPO_LINK in the traversal, brain_impact / blast_radius silently
    /// miss every downstream cross-repo consumer.
    #[test]
    fn impact_includes_cross_repo_link_callers() {
        use nestweaver_schema::{CrossRepoLinkType, EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("target", "ApiHandler"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("consumer", "RemoteCaller"))
            .unwrap();

        // A consumer in another repo depends on `target` via a cross-repo link.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "consumer".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: Vec::new(),
            })
            .unwrap();

        let impacted = store.impact("target", 5, 0.0).unwrap();
        assert!(
            impacted.iter().any(|n| n.uid == "consumer"),
            "impact must surface cross-repo callers linked via CROSS_REPO_LINK; got: {:?}",
            impacted.iter().map(|n| n.uid.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn outgoing_impact_edges_include_structural_edges() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("derived", "DerivedHandler"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("base", "BaseHandler"))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "derived".to_string(),
                target_uid: "base".to_string(),
                edge_type: EdgeType::Extends,
                confidence: 0.88,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let edges = store.outgoing_impact_edges("derived", 0.0).unwrap();
        assert!(
            edges.iter().any(|edge| {
                edge.target_uid == "base"
                    && edge.edge_type == "EXTENDS_SYM"
                    && (edge.confidence - 0.88).abs() < f32::EPSILON
            }),
            "outgoing impact helper must match traversal edge types; got: {edges:?}"
        );
    }

    /// A pre-tripped cancel flag must make the impact BFS return `Cancelled`
    /// on its first dequeue — never a (truncated) Ok result.
    #[test]
    fn impact_cancellable_bails_when_flag_is_set() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("target", "ApiHandler"))
            .unwrap();

        let cancel = Arc::new(AtomicBool::new(true));
        let err = store
            .impact_cancellable("target", 5, 0.0, Some(&cancel))
            .expect_err("pre-cancelled impact must return Err, not a truncated result");
        assert!(
            err.is_cancelled(),
            "expected StoreError::Cancelled, got: {err}"
        );

        // Untripped flag: byte-for-byte the original behavior.
        let untripped = Arc::new(AtomicBool::new(false));
        assert!(
            store
                .impact_cancellable("target", 5, 0.0, Some(&untripped))
                .is_ok()
        );
    }

    /// Convenience: empty [`SeedResolutionConfig`] for legacy cache tests
    /// where path/kind ranking is irrelevant.
    fn no_rules() -> SeedResolutionConfig {
        SeedResolutionConfig {
            path_deboost: Vec::new(),
            kind_priority: Vec::new(),
        }
    }

    /// Verify that `search_symbols_by_name` returns consistent results across
    /// two calls and that the second call is served from the cache.
    #[test]
    fn symbol_name_cache_returns_same_results_on_second_call() {
        let store = GraphStore::in_memory().unwrap();

        store.insert_symbol(&make_symbol("s1", "FooBar")).unwrap();
        store.insert_symbol(&make_symbol("s2", "FooQux")).unwrap();
        store.insert_symbol(&make_symbol("s3", "BazBaz")).unwrap();

        // First call — cache miss, queries DB.
        let first = store
            .search_symbols_by_name("foo", 10, &no_rules())
            .unwrap();
        assert_eq!(first.len(), 2, "expected FooBar and FooQux");

        // Second call — should hit the cache. Results must be identical.
        let second = store
            .search_symbols_by_name("foo", 10, &no_rules())
            .unwrap();
        assert_eq!(second.len(), 2, "cached call must return the same count");

        // The UIDs returned by both calls must be the same set.
        let mut uids_first: Vec<&str> = first.iter().map(|s| s.uid.as_str()).collect();
        let mut uids_second: Vec<&str> = second.iter().map(|s| s.uid.as_str()).collect();
        uids_first.sort_unstable();
        uids_second.sort_unstable();
        assert_eq!(uids_first, uids_second);

        // Verify the cache is populated.
        {
            let guard = store.symbol_name_cache.lock().unwrap();
            assert!(guard.is_some(), "cache must be populated after first call");
            assert_eq!(
                guard.as_ref().unwrap().symbols.len(),
                3,
                "cache must hold all inserted symbols"
            );
        }
    }

    /// After bumping `graph_generation`, the next call must re-query the DB
    /// (cache miss) and reflect the new symbol table.
    #[test]
    fn symbol_name_cache_invalidated_on_generation_bump() {
        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&make_symbol("s1", "Alpha")).unwrap();

        let first = store
            .search_symbols_by_name("alpha", 10, &no_rules())
            .unwrap();
        assert_eq!(first.len(), 1);

        // Simulate a reindex bump.
        store.bump_graph_generation();

        // Insert a second symbol that would have been picked up by "alpha" —
        // it won't match, but we add a new "AlphaBeta" that should appear.
        store
            .insert_symbol(&make_symbol("s2", "AlphaBeta"))
            .unwrap();

        let second = store
            .search_symbols_by_name("alpha", 10, &no_rules())
            .unwrap();
        assert_eq!(
            second.len(),
            2,
            "after generation bump the cache should be refreshed and include the new symbol"
        );
    }

    // ── Finding #7 — seed_resolution scoring + kind tiebreak ────────────

    #[test]
    fn path_factor_with_no_rules_is_one() {
        assert!((compute_path_factor("playwright/foo.ts", &[]) - 1.0).abs() < 1e-9);
        assert!((compute_path_factor("src/main.rs", &[]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn path_factor_applies_prefix_rule() {
        let rules = vec![PathDeboostRule {
            prefix: Some("/playwright/".into()),
            suffix: None,
            factor: 0.2,
        }];
        assert!((compute_path_factor("playwright/pages/foo.ts", &rules) - 0.2).abs() < 1e-9);
        // Non-matching path is untouched.
        assert!((compute_path_factor("src/main.rs", &rules) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn path_factor_applies_suffix_rule() {
        let rules = vec![PathDeboostRule {
            prefix: None,
            suffix: Some(".test.ts".into()),
            factor: 0.5,
        }];
        assert!((compute_path_factor("src/foo.test.ts", &rules) - 0.5).abs() < 1e-9);
        // Suffix is case-sensitive on the raw file_path.
        assert!((compute_path_factor("src/foo.TEST.ts", &rules) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn path_factor_multiplies_when_both_match() {
        let rules = vec![
            PathDeboostRule {
                prefix: Some("/playwright/".into()),
                suffix: None,
                factor: 0.2,
            },
            PathDeboostRule {
                prefix: None,
                suffix: Some(".test.ts".into()),
                factor: 0.5,
            },
        ];
        // 0.2 * 0.5 = 0.1
        assert!((compute_path_factor("playwright/foo.test.ts", &rules) - 0.1).abs() < 1e-9);
    }

    #[test]
    fn path_factor_case_insensitive_for_prefix() {
        let rules = vec![PathDeboostRule {
            prefix: Some("/playwright/".into()),
            suffix: None,
            factor: 0.2,
        }];
        assert!((compute_path_factor("Playwright/Pages/Foo.ts", &rules) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn search_prefers_class_over_property_on_same_name_when_paths_match_evenly() {
        let store = GraphStore::in_memory().unwrap();

        let mut class_sym = make_symbol("sym-class", "MyWidget");
        class_sym.kind = SymbolKind::Class;
        class_sym.file_path = "src/main.ts".to_string();
        store.insert_symbol(&class_sym).unwrap();

        let mut prop_sym = make_symbol("sym-prop", "myWidget");
        prop_sym.kind = SymbolKind::Property;
        prop_sym.file_path = "src/helpers.ts".to_string();
        store.insert_symbol(&prop_sym).unwrap();

        // Default kind_priority puts Class above Property; neither path
        // matches any deboost rule (no /test/ etc. segment).
        let cfg = SeedResolutionConfig {
            path_deboost: Vec::new(),
            kind_priority: default_kind_priority(),
        };
        let results = store.search_symbols_by_name("mywidget", 10, &cfg).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].uid, "sym-class",
            "Class must outrank Property under default kind_priority"
        );
    }

    #[test]
    fn search_prefers_production_over_playwright_for_same_lowercased_name() {
        let store = GraphStore::in_memory().unwrap();

        let mut prod = make_symbol("sym-prod", "MyWidget");
        prod.kind = SymbolKind::Class;
        prod.file_path = "src/components/MyWidget.tsx".to_string();
        store.insert_symbol(&prod).unwrap();

        for (i, p) in [
            "playwright/components/MyWidget.spec.ts",
            "playwright/regression/MyWidget.spec.ts",
            "playwright/smoke/MyWidget.spec.ts",
        ]
        .iter()
        .enumerate()
        {
            let mut s = make_symbol(&format!("sym-pw-{i}"), "MyWidget");
            s.kind = SymbolKind::Class;
            s.file_path = (*p).to_string();
            store.insert_symbol(&s).unwrap();
        }

        // With default deboost: prod = 4.0*1.0 = 4.0, playwright = 4.0*0.2 = 0.8.
        let cfg = SeedResolutionConfig::default();
        let results = store.search_symbols_by_name("MyWidget", 10, &cfg).unwrap();
        assert_eq!(
            results[0].file_path, "src/components/MyWidget.tsx",
            "production code must outrank playwright/ tests under default deboost"
        );
    }

    #[test]
    fn search_kind_priority_overridden_by_config() {
        let store = GraphStore::in_memory().unwrap();

        let mut class_sym = make_symbol("sym-class", "MyWidget");
        class_sym.kind = SymbolKind::Class;
        class_sym.file_path = "src/main.ts".to_string();
        store.insert_symbol(&class_sym).unwrap();

        let mut prop_sym = make_symbol("sym-prop", "myWidget");
        prop_sym.kind = SymbolKind::Property;
        prop_sym.file_path = "src/helpers.ts".to_string();
        store.insert_symbol(&prop_sym).unwrap();

        // Override kind_priority so Property wins (override is full list,
        // not append). Paths factor in evenly.
        let cfg = SeedResolutionConfig {
            path_deboost: Vec::new(),
            kind_priority: vec![
                "Property".into(),
                "Class".into(),
                "Interface".into(),
                "TypeAlias".into(),
                "Method".into(),
                "Function".into(),
                "Constant".into(),
                "Variable".into(),
                "Module".into(),
                "Enum".into(),
                "Trait".into(),
                "Extension".into(),
            ],
        };
        let results = store.search_symbols_by_name("mywidget", 10, &cfg).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].uid, "sym-prop",
            "Property must win when kind_priority places it first"
        );
    }

    // ── kind_rank sanity ────────────────────────────────────────────────

    #[test]
    fn kind_rank_returns_index_for_known_kinds() {
        let priority = default_kind_priority();
        assert_eq!(kind_rank(SymbolKind::Class, &priority), 0);
        assert!(
            kind_rank(SymbolKind::Property, &priority) > kind_rank(SymbolKind::Class, &priority)
        );
    }

    #[test]
    fn kind_rank_returns_max_for_kinds_missing_from_priority() {
        let priority = vec!["Class".to_string()];
        assert_eq!(kind_rank(SymbolKind::Class, &priority), 0);
        assert_eq!(kind_rank(SymbolKind::Function, &priority), usize::MAX);
    }

    // ── impact_detailed — truncation honesty flags ──────────────────────

    use super::IMPACT_EDGE_TYPES;

    /// A chain fully contained within `max_depth`, all high-confidence, is a
    /// complete walk: neither truncation flag should fire.
    #[test]
    fn impact_detailed_complete_walk_sets_no_flags() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["target", "a", "b"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // b → a → target (callers point at their callee).
        for (src, tgt) in [("a", "target"), ("b", "a")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: tgt.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: Vec::new(),
                })
                .unwrap();
        }

        let result = store
            .impact_detailed("target", 5, 0.0, IMPACT_EDGE_TYPES, None)
            .unwrap();
        assert!(
            !result.truncated_by_threshold,
            "no path was pruned below threshold"
        );
        assert!(
            !result.truncated_by_depth,
            "no frontier node hit the depth boundary"
        );
        assert_eq!(result.nodes.len(), 2, "both a and b are reachable");
    }

    /// A chain longer than `max_depth` leaves a frontier node unexpanded at the
    /// boundary — `truncated_by_depth` must fire.
    #[test]
    fn impact_detailed_flags_depth_truncation() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["target", "a", "b", "c"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        for (src, tgt) in [("a", "target"), ("b", "a"), ("c", "b")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: tgt.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: Vec::new(),
                })
                .unwrap();
        }

        let result = store
            .impact_detailed("target", 2, 0.0, IMPACT_EDGE_TYPES, None)
            .unwrap();
        assert!(
            result.truncated_by_depth,
            "a node reached max_depth and was left unexpanded"
        );
        assert!(
            !result.truncated_by_threshold,
            "all confidences are high; nothing pruned by threshold"
        );
    }

    /// A chain whose decayed score falls below `DEFAULT_IMPACT_THRESHOLD`
    /// (0.10) must set `truncated_by_threshold`.
    #[test]
    fn impact_detailed_flags_threshold_truncation() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["target", "a", "b"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // 1.0 * 0.3 = 0.30 (kept); 0.30 * 0.3 = 0.09 (< 0.10, pruned).
        for (src, tgt) in [("a", "target"), ("b", "a")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: tgt.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.3,
                    link_type: None,
                    evidence: Vec::new(),
                })
                .unwrap();
        }

        let result = store
            .impact_detailed("target", 5, 0.0, IMPACT_EDGE_TYPES, None)
            .unwrap();
        assert!(
            result.truncated_by_threshold,
            "the b→a path decays below the impact threshold and is pruned"
        );
        assert!(
            !result.truncated_by_depth,
            "the boundary was never reached (b never enqueued)"
        );
        assert_eq!(
            result.nodes.len(),
            1,
            "only a survives; b is pruned below threshold"
        );
    }

    /// A restricted `edges` set is honored: only the listed edge types are
    /// traversed, and `edge_types` echoes the set actually used.
    #[test]
    fn impact_detailed_restricts_to_requested_edges() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["target", "caller", "importer"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // One dependent reaches `target` via CALLS, another only via IMPORTS.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "caller".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "importer".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.9,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let result = store
            .impact_detailed("target", 5, 0.0, &[EdgeType::Calls], None)
            .unwrap();
        assert_eq!(
            result.edge_types,
            vec![EdgeType::Calls],
            "edge_types must echo the requested set"
        );
        let uids: Vec<&str> = result.nodes.iter().map(|n| n.uid.as_str()).collect();
        assert!(uids.contains(&"caller"), "CALLS dependent must be included");
        assert!(
            !uids.contains(&"importer"),
            "IMPORTS-only dependent must be excluded when only CALLS is traversed"
        );
    }
}
