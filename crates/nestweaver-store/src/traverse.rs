use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use nestweaver_schema::{EdgeType, SymbolKind};

use crate::db::GraphStore;
use crate::error::StoreError;
use crate::ranking::{PathDeboostRule, SeedResolutionConfig};
use crate::tantivy_index::SearchTotal;

/// Maximum precision of symbol-match totals. Ranking still scans the cached
/// symbol snapshot so late high-quality hits are not missed, but only the
/// caller-requested top results are retained. Reaching this cap reports a
/// lower bound instead of an unbounded exact-match collection.
pub const SYMBOL_SEARCH_COUNT_CAP: usize = 100_000;

/// A ranked symbol page with an independently bounded match total.
#[derive(Debug, Clone)]
pub struct SymbolSearchPage {
    pub symbols: Vec<nestweaver_schema::Symbol>,
    pub total: SearchTotal,
}

struct RankedSymbol<'a> {
    adjusted_score: f64,
    kind_rank: usize,
    file_path: &'a str,
    ordinal: usize,
    symbol: &'a nestweaver_schema::Symbol,
}

impl PartialEq for RankedSymbol<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.ordinal == other.ordinal
    }
}

impl Eq for RankedSymbol<'_> {}

impl PartialOrd for RankedSymbol<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedSymbol<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // "Less" means better-ranked. BinaryHeap therefore keeps the worst
        // retained candidate at peek(), making bounded top-K replacement O(log K).
        other
            .adjusted_score
            .partial_cmp(&self.adjusted_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| self.kind_rank.cmp(&other.kind_rank))
            .then_with(|| self.file_path.cmp(other.file_path))
            // Preserve stable DB/cache order for otherwise identical keys.
            .then_with(|| self.ordinal.cmp(&other.ordinal))
    }
}

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
/// Dirty index publications never consume or populate this cache, even when
/// the live dirty generation happens to match an entry.
pub(crate) struct SymbolNameCached {
    /// The `graph_generation` value at cache-fill time.
    pub generation: u64,
    /// All symbols together with their pre-lowercased names for O(n) contains
    /// matching without re-allocating on every call.
    pub symbols: Vec<(String, nestweaver_schema::Symbol)>,
}

/// Minimum impact score for a node to be included in traversal results.
/// Edges below this threshold are pruned during BFS.
///
/// Deliberately aggressive: at 0.10 a depth-4 chain of 0.5-confidence edges
/// (score 0.0625) is pruned. That default is kept — not lowered — because the
/// multiplicative decay means genuinely-attributable impact fades fast, the
/// prune bounds BFS fan-out on dense graphs, and the value is mirrored by the
/// in-memory walk (`affected_tests::IMPACT_THRESHOLD`) so the two must not
/// drift. The trade-off is surfaced, not hidden: a prune sets
/// [`ImpactResult::truncated_by_threshold`], and callers that need the full
/// traversal can opt out via [`GraphStore::impact_with_flags_and_threshold`]
/// (CLI: `impact --min-score 0`).
pub const DEFAULT_IMPACT_THRESHOLD: f64 = 0.10;

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
///
/// The truncation flags are deliberately *pessimistic*: they fire when the walk
/// *could* have dropped a dependent (a pruned path, an unexpanded frontier),
/// even if nothing was actually missed. This one-sided bias is intentional — an
/// over-fired flag costs a needless "review manually", a missed one lets a
/// degraded run read as "safe". Do not "tighten" them to only fire on proven
/// loss; the whole trust model (blast-radius `DEGRADED-UNKNOWN`) depends on them
/// over-approximating incompleteness.
#[derive(Debug, Clone)]
pub struct ImpactResult {
    pub nodes: Vec<ImpactNode>,
    /// A path was pruned because its decayed score fell below the impact
    /// threshold — the tail of the impact set may be incomplete. Pessimistic:
    /// set whenever a prune happened, not only when it hid a real dependent.
    pub truncated_by_threshold: bool,
    /// A frontier node was reached at `max_depth` and left unexpanded —
    /// deeper dependents may exist beyond the returned set. Pessimistic: set on
    /// any capped frontier, even if nothing lay beyond it.
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

#[derive(Debug, Clone)]
struct SnapshotCaller {
    uid: String,
    confidence: f64,
}

/// An immutable reverse-adjacency view of the symbol graph.
///
/// GraphStore impact entry points use this as their clean-generation default.
/// Building it fails closed if a scan cannot be reconciled with primary-key
/// symbol lookups or if an edge has invalid confidence. A generation-keyed,
/// single-flight cache amortizes construction; dirty or raced publications
/// retain the live database traversal for that request.
#[derive(Debug, Clone)]
pub struct ImpactSnapshot {
    symbols_by_uid: HashMap<String, nestweaver_schema::Symbol>,
    callers_by_target: HashMap<String, HashMap<String, Vec<SnapshotCaller>>>,
}

struct ImpactEdgePlan<'a> {
    structural: &'a [EdgeType],
    combined: Vec<EdgeType>,
    data_active: bool,
    data_max_depth: u32,
}

impl<'a> ImpactEdgePlan<'a> {
    fn new(structural: &'a [EdgeType], data: &[EdgeType], data_max_depth: u32) -> Self {
        let data_active = data_max_depth > 0 && !data.is_empty();
        let combined = if data_active {
            structural
                .iter()
                .copied()
                .chain(data.iter().copied())
                .collect()
        } else {
            Vec::new()
        };
        Self {
            structural,
            combined,
            data_active,
            data_max_depth,
        }
    }

    fn prepare_set(&self) -> &[EdgeType] {
        if self.data_active {
            &self.combined
        } else {
            self.structural
        }
    }

    fn edges_at_depth(&self, depth: u32) -> &[EdgeType] {
        if self.data_active && depth < self.data_max_depth {
            &self.combined
        } else {
            self.structural
        }
    }

    fn result_edge_types(&self) -> Vec<EdgeType> {
        if self.data_active {
            self.combined.clone()
        } else {
            self.structural.to_vec()
        }
    }
}

impl ImpactSnapshot {
    /// Traverse this snapshot with the same structural and shallow data-edge
    /// policy as [`GraphStore::impact_with_data_edges`].
    pub fn impact_with_data_edges(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        data_max_depth: u32,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        self.impact_bfs(
            target_uid,
            max_depth,
            min_confidence,
            IMPACT_EDGE_TYPES,
            IMPACT_DATA_EDGE_TYPES,
            data_max_depth,
            DEFAULT_IMPACT_THRESHOLD,
            None,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn impact_bfs(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        structural: &[EdgeType],
        data: &[EdgeType],
        data_max_depth: u32,
        min_score: f64,
        allowed_symbols: Option<&HashSet<String>>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        let plan = ImpactEdgePlan::new(structural, data, data_max_depth);
        run_impact_walk(
            target_uid,
            max_depth,
            min_confidence,
            &plan,
            min_score,
            allowed_symbols,
            cancel,
            |uid, min_confidence, edges| self.direct_callers(uid, min_confidence, edges),
        )
    }

    fn direct_callers(&self, uid: &str, min_confidence: f32, edges: &[EdgeType]) -> Vec<CallerRow> {
        let Some(by_edge) = self.callers_by_target.get(uid) else {
            return Vec::new();
        };
        let min_confidence = min_confidence as f64;
        let mut callers = Vec::new();
        // Preserve the requested edge-type order used by the live prepared
        // statements. Equal-score paths therefore keep the same first winner.
        for edge in edges {
            let edge_label = edge.rel_table_name();
            let Some(rows) = by_edge.get(edge_label) else {
                continue;
            };
            callers.extend(
                rows.iter()
                    .filter(|row| row.confidence >= min_confidence)
                    .map(|row| {
                        let symbol = self
                            .symbols_by_uid
                            .get(&row.uid)
                            .expect("impact snapshot adjacency must reference a validated symbol");
                        CallerRow {
                            uid: row.uid.clone(),
                            name: symbol.name.clone(),
                            file_path: symbol.file_path.clone(),
                            start_line: symbol.start_line,
                            edge_type: edge_label.to_string(),
                            confidence: row.confidence as f32,
                        }
                    }),
            );
        }
        callers
    }
}

/// Structural reverse-impact edge set. The confidence-weighted reverse BFS
/// (`impact_bfs`) and any in-memory equivalent (e.g. `affected_tests`) must use
/// exactly this set, so it is public to keep the two implementations from
/// drifting.
pub const IMPACT_EDGE_TYPES: &[EdgeType] = &[
    EdgeType::Calls,
    EdgeType::Imports,
    EdgeType::Extends,
    EdgeType::Implements,
    EdgeType::Includes,
    EdgeType::CrossRepoLink,
];

/// Data-dependence edges: a symbol references a changed type (`Uses`) or reads/
/// writes a changed field/property (`Accesses`). Followed only to a shallow
/// depth because they fan out heavily.
pub const IMPACT_DATA_EDGE_TYPES: &[EdgeType] = &[EdgeType::Uses, EdgeType::Accesses];

#[allow(clippy::too_many_arguments)]
fn run_impact_walk(
    target_uid: &str,
    max_depth: u32,
    min_confidence: f32,
    plan: &ImpactEdgePlan<'_>,
    min_score: f64,
    allowed_symbols: Option<&HashSet<String>>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    mut direct_callers: impl FnMut(&str, f32, &[EdgeType]) -> Vec<CallerRow>,
) -> Result<ImpactResult, StoreError> {
    let mut scores: HashMap<String, f64> = HashMap::new();
    scores.insert(target_uid.to_string(), 1.0);

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((target_uid.to_string(), 0));

    let mut result_map: HashMap<String, ImpactNode> = HashMap::new();
    let mut truncated_by_threshold = false;
    let mut truncated_by_depth = false;

    while let Some((current_uid, depth)) = queue.pop_front() {
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
            // The shared cancel flag is a bare bool and cannot carry a reason,
            // so the leaf always reports Timeout (see CancelReason).
            return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
        }
        if depth >= max_depth {
            // A frontier node reached the depth boundary unexpanded; deeper
            // dependents may exist beyond the returned set.
            truncated_by_depth = true;
            continue;
        }

        let parent_score = scores.get(&current_uid).copied().unwrap_or(0.0);
        let callers = direct_callers(&current_uid, min_confidence, plan.edges_at_depth(depth));
        for row in callers {
            if row.uid == target_uid {
                continue;
            }
            if allowed_symbols.is_some_and(|allowed| !allowed.contains(&row.uid)) {
                continue;
            }

            let candidate_score = parent_score * row.confidence as f64;
            if candidate_score < min_score {
                truncated_by_threshold = true;
                continue;
            }

            let prev_score = scores.get(&row.uid).copied().unwrap_or(0.0);
            if candidate_score > prev_score {
                scores.insert(row.uid.clone(), candidate_score);
                result_map.insert(
                    row.uid.clone(),
                    ImpactNode {
                        uid: row.uid.clone(),
                        name: row.name,
                        file_path: row.file_path,
                        start_line: row.start_line,
                        edge_type: row.edge_type,
                        confidence: row.confidence,
                        depth: depth + 1,
                        impact_score: candidate_score,
                    },
                );
                // Re-enqueue so downstream nodes can inherit an improved
                // max-product score.
                queue.push_back((row.uid, depth + 1));
            }
        }
    }

    let mut nodes: Vec<ImpactNode> = result_map.into_values().collect();
    nodes.sort_by(|left, right| {
        right
            .impact_score
            .partial_cmp(&left.impact_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.uid.cmp(&right.uid))
    });

    Ok(ImpactResult {
        nodes,
        truncated_by_threshold,
        truncated_by_depth,
        edge_types: plan.result_edge_types(),
    })
}

impl GraphStore {
    /// Build a corruption-checked reverse-adjacency snapshot without selecting
    /// it as the default impact implementation.
    ///
    /// Strict scans of the impact edge tables supply the distinct endpoint
    /// primary keys. Every participating symbol is then re-read through an
    /// exact `UNWIND`-driven batch of primary-key probes before any display
    /// fields can enter the snapshot. This intentionally avoids a filtered
    /// `Symbol` scan: a prior optimization could align a valid edge with the
    /// wrong scanned symbol row. The expected key set is reconciled against
    /// every result row. Any edge-table query failure, missing or unexpected
    /// symbol, duplicate key, dangling symbol edge, or non-finite/out-of-range
    /// confidence fails the entire build rather than producing a deceptively
    /// incomplete impact graph. Symbols with no impact edges are omitted
    /// because they cannot appear in an impact result.
    ///
    /// # Performance
    ///
    /// Snapshot construction still scales with the full impact-edge endpoint
    /// set and can be expensive on large graphs. The default impact entry
    /// points therefore construct it at most once per clean graph generation.
    /// This raw builder remains public for validation and explicit snapshots.
    pub fn load_impact_snapshot(&self) -> Result<ImpactSnapshot, StoreError> {
        self.load_impact_snapshot_cancellable(None)
    }

    fn load_impact_snapshot_cancellable(
        &self,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactSnapshot, StoreError> {
        let conn = self.conn()?;
        let mut snapshot_edges = Vec::new();
        let mut required_uids = HashSet::new();
        for edge_type in IMPACT_EDGE_TYPES.iter().chain(IMPACT_DATA_EDGE_TYPES) {
            if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
                return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
            }
            let edge_label = edge_type.rel_table_name();
            let query = format!(
                "MATCH (source:Symbol)-[edge:{edge_label}]->(target:Symbol) \
                 RETURN source.uid, target.uid, edge.confidence"
            );
            let rows = conn.query(&query).map_err(|error| {
                StoreError::Query(format!(
                    "impact snapshot failed to load {edge_label} edges: {error}"
                ))
            })?;
            for row in rows {
                if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
                    return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
                }
                let source_uid = crate::read::extract_string(&row, 0)?;
                let target_uid = crate::read::extract_string(&row, 1)?;
                let confidence = match row.get(2) {
                    Some(lbug::Value::Double(value)) => *value,
                    Some(lbug::Value::Float(value)) => *value as f64,
                    Some(lbug::Value::Int64(value)) => *value as f64,
                    Some(lbug::Value::Null(_)) | None => {
                        return Err(StoreError::Query(format!(
                            "impact snapshot missing confidence on \
                             {source_uid} -[{edge_label}]-> {target_uid}"
                        )));
                    }
                    Some(value) => {
                        return Err(StoreError::Query(format!(
                            "impact snapshot expected numeric confidence on \
                             {source_uid} -[{edge_label}]-> {target_uid}, got {value:?}"
                        )));
                    }
                };
                if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
                    return Err(StoreError::Query(format!(
                        "impact snapshot invalid confidence {confidence} on \
                         {source_uid} -[{edge_label}]-> {target_uid}"
                    )));
                }
                required_uids.insert(source_uid.clone());
                required_uids.insert(target_uid.clone());
                snapshot_edges.push((source_uid, target_uid, edge_label.to_string(), confidence));
            }
        }

        let mut required_uids: Vec<String> = required_uids.into_iter().collect();
        required_uids.sort();
        let required_uid_refs: Vec<&str> = required_uids.iter().map(String::as_str).collect();
        drop(conn);
        let symbols_by_uid = self
            .batch_lookup_symbols_exact(&required_uid_refs)
            .map_err(|error| {
                StoreError::Query(format!(
                    "impact snapshot exact bulk symbol lookup failed: {error}"
                ))
            })?;

        let mut callers_by_target: HashMap<String, HashMap<String, Vec<SnapshotCaller>>> =
            HashMap::new();
        for (source_uid, target_uid, edge_type, confidence) in snapshot_edges {
            let caller = symbols_by_uid.get(&source_uid).ok_or_else(|| {
                StoreError::Query(format!(
                    "impact snapshot edge references missing source symbol: {source_uid}"
                ))
            })?;
            if !symbols_by_uid.contains_key(&target_uid) {
                return Err(StoreError::Query(format!(
                    "impact snapshot edge references missing target symbol: {target_uid}"
                )));
            }
            callers_by_target
                .entry(target_uid)
                .or_default()
                .entry(edge_type)
                .or_default()
                .push(SnapshotCaller {
                    uid: caller.uid.clone(),
                    confidence,
                });
        }

        Ok(ImpactSnapshot {
            symbols_by_uid,
            callers_by_target,
        })
    }

    fn cached_impact_snapshot(
        &self,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Option<std::sync::Arc<ImpactSnapshot>>, StoreError> {
        self.cached_impact_snapshot_with_loader(cancel, || {
            self.load_impact_snapshot_cancellable(cancel)
        })
    }

    fn cached_impact_snapshot_with_loader(
        &self,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
        loader: impl FnOnce() -> Result<ImpactSnapshot, StoreError>,
    ) -> Result<Option<std::sync::Arc<ImpactSnapshot>>, StoreError> {
        let cancelled =
            || cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed));
        if cancelled() {
            return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
        }

        // Acquire before checking the cache so concurrent first-touch callers
        // queue here, then observe the one snapshot published by the elected
        // loader instead of issuing duplicate full edge-table scans.
        let _fill = loop {
            match self.impact_snapshot_compute_lock.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if cancelled() {
                        return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
                    }
                    // Snapshot construction takes seconds on production-sized
                    // graphs. Polling at a small interval keeps query
                    // cancellation responsive without spinning a core.
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        };
        if cancelled() {
            return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
        }

        let query_generation = {
            // Cache-hit validation and index publication transitions share this
            // barrier, making the decision linearizable with dirty-marker and
            // generation changes.
            let _publication = self
                .pagerank_compute_lock
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let generation = self.graph_generation();
            if self.is_index_publication_dirty() {
                return Ok(None);
            }
            if let Some(snapshot) = self
                .impact_snapshot_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .filter(|(cached_generation, _)| *cached_generation == generation)
                .map(|(_, snapshot)| std::sync::Arc::clone(snapshot))
            {
                return Ok(Some(snapshot));
            }
            generation
        };

        let candidate = std::sync::Arc::new(loader()?);
        if cancelled() {
            return Err(StoreError::Cancelled(crate::error::CancelReason::Timeout));
        }

        let _publication = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current_generation = self.graph_generation();
        if self.is_index_publication_dirty() || current_generation != query_generation {
            // The graph changed while the snapshot loaded. Never publish or
            // serve a candidate that cannot be proven current; the caller keeps
            // the existing live traversal for this raced request.
            return Ok(None);
        }

        let mut cached = self
            .impact_snapshot_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(snapshot) = cached
            .as_ref()
            .filter(|(generation, _)| *generation == current_generation)
            .map(|(_, snapshot)| std::sync::Arc::clone(snapshot))
        {
            return Ok(Some(snapshot));
        }
        *cached = Some((current_generation, std::sync::Arc::clone(&candidate)));
        Ok(Some(candidate))
    }

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

    /// Like [`impact_cancellable`](Self::impact_cancellable), but traverses only
    /// the subgraph induced by `allowed_symbols`. Disallowed callers are not
    /// returned or expanded, so an allowed node reachable only through a
    /// disallowed intermediate cannot reveal that hidden topology.
    #[deprecated(
        note = "drops the truncation-honesty flags (threshold/depth pruning) from the \
                underlying ImpactResult; use `impact_with_flags_within` instead so callers \
                can see when the walk was pruned"
    )]
    pub fn impact_cancellable_within(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        allowed_symbols: &HashSet<String>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Vec<ImpactNode>, StoreError> {
        Ok(self
            .impact_bfs(
                target_uid,
                max_depth,
                min_confidence,
                IMPACT_EDGE_TYPES,
                &[],
                0,
                DEFAULT_IMPACT_THRESHOLD,
                Some(allowed_symbols),
                cancel,
            )?
            .nodes)
    }

    /// Scoped impact (authz allow-list) returning the full ImpactResult with
    /// truncation-honesty flags — the flags-dropping `impact_cancellable_within`
    /// hides threshold pruning from callers (surfaced as a silent-floor bug
    /// in the CLI/MCP impact paths).
    pub fn impact_with_flags_within(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        allowed_symbols: &HashSet<String>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        self.impact_bfs(
            target_uid,
            max_depth,
            min_confidence,
            IMPACT_EDGE_TYPES,
            &[],
            0,
            DEFAULT_IMPACT_THRESHOLD,
            Some(allowed_symbols),
            cancel,
        )
    }

    /// Impact with the default edge set, returning the ImpactResult (with
    /// truncation-honesty flags). Convenience over `impact_detailed`.
    pub fn impact_with_flags(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        self.impact_detailed(
            target_uid,
            max_depth,
            min_confidence,
            IMPACT_EDGE_TYPES,
            cancel,
        )
    }

    /// Like [`impact_with_flags`](Self::impact_with_flags), but with an explicit
    /// score-pruning threshold instead of [`DEFAULT_IMPACT_THRESHOLD`]. Pass
    /// `0.0` to disable score pruning entirely and get the full traversal
    /// (bounded only by `max_depth`); the `truncated_by_threshold` flag then
    /// never fires. Intended for the CLI `--min-score` opt-out — the default
    /// threshold stays in place everywhere else (see its doc comment).
    pub fn impact_with_flags_and_threshold(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        threshold: f64,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        self.impact_bfs(
            target_uid,
            max_depth,
            min_confidence,
            IMPACT_EDGE_TYPES,
            &[],
            0,
            threshold,
            None,
            cancel,
        )
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
        // Structural-only walk: no data-dependence edges (empty set, cap 0).
        self.impact_bfs(
            target_uid,
            max_depth,
            min_confidence,
            edges,
            &[],
            0,
            DEFAULT_IMPACT_THRESHOLD,
            None,
            cancel,
        )
    }

    /// Impact with the structural edge set (to `max_depth`) plus data-dependence
    /// edges followed only while depth < `data_max_depth`. Data edges are shallow-
    /// capped because type-reference/field-access edges approach full program
    /// slices if followed transitively.
    pub fn impact_with_data_edges(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        data_max_depth: u32,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        self.impact_bfs(
            target_uid,
            max_depth,
            min_confidence,
            IMPACT_EDGE_TYPES,
            IMPACT_DATA_EDGE_TYPES,
            data_max_depth,
            DEFAULT_IMPACT_THRESHOLD,
            None,
            cancel,
        )
    }

    /// Shared confidence-weighted reverse BFS backing both [`impact_detailed`]
    /// (structural-only) and [`impact_with_data_edges`] (structural + shallow
    /// data-dependence tier).
    ///
    /// `structural` edges are followed to `max_depth`; `data` edges are followed
    /// only at depths `d < data_max_depth`, because type-reference/field-access
    /// edges fan out toward full program slices if followed transitively. The
    /// combined edge slice is precomputed once and selected per depth: combined
    /// while `d < data_max_depth`, structural-only beyond. When `data` is empty
    /// or `data_max_depth == 0` this is byte-for-byte the structural-only walk.
    /// `min_score` is the score-pruning threshold: a path whose decayed score
    /// falls below it is pruned (and flags `truncated_by_threshold`). All
    /// existing entry points pass [`DEFAULT_IMPACT_THRESHOLD`]; pass `0.0` to
    /// disable score pruning.
    #[allow(clippy::too_many_arguments)]
    fn impact_bfs(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        structural: &[EdgeType],
        data: &[EdgeType],
        data_max_depth: u32,
        min_score: f64,
        allowed_symbols: Option<&HashSet<String>>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        match self.cached_impact_snapshot(cancel) {
            Ok(Some(snapshot)) => {
                return snapshot.impact_bfs(
                    target_uid,
                    max_depth,
                    min_confidence,
                    structural,
                    data,
                    data_max_depth,
                    min_score,
                    allowed_symbols,
                    cancel,
                );
            }
            Ok(None) => {}
            Err(error @ StoreError::Cancelled(_)) => return Err(error),
            Err(_) => {
                // The snapshot is an optimization. A malformed full-symbol
                // projection or other construction-only failure must not
                // remove callers that the established live traversal can
                // still read safely.
                tracing::warn!(
                    "impact snapshot construction failed; using live traversal for this request"
                );
            }
        }
        self.impact_bfs_live(
            target_uid,
            max_depth,
            min_confidence,
            structural,
            data,
            data_max_depth,
            min_score,
            allowed_symbols,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn impact_bfs_live(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
        structural: &[EdgeType],
        data: &[EdgeType],
        data_max_depth: u32,
        min_score: f64,
        allowed_symbols: Option<&HashSet<String>>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<ImpactResult, StoreError> {
        // nw-065: one connection and one prepared statement per edge type for
        // the WHOLE traversal. Previously each visited node created a
        // connection and re-prepared every statement, which dominated
        // traversal cost. Prepare the union of structural+data edges; the
        // per-depth edge slice still selects which are actually followed.
        let plan = ImpactEdgePlan::new(structural, data, data_max_depth);
        let conn = self.conn()?;
        let mut stmts = Self::prepare_caller_stmts(&conn, plan.prepare_set());
        let mut result = run_impact_walk(
            target_uid,
            max_depth,
            min_confidence,
            &plan,
            min_score,
            allowed_symbols,
            cancel,
            |uid, min_confidence, edges| {
                Self::direct_callers_prepared(&conn, &mut stmts, uid, min_confidence, edges)
            },
        )?;

        // Repair display fields from PRIMARY-KEY point lookups.
        //
        // The traversal reads caller rows through a pattern scan, and the
        // storage engine can return garbled non-PK string values from partial
        // string scans after delete+checkpoint cycles (which re-indexing
        // performs routinely) — while PK point lookups return the correct
        // values for the very same rows. `uid` is the primary key and is
        // therefore trustworthy; `name`/`file_path` are not. Re-resolve them
        // through the PK-driven batch lookup so no consumer of impact results
        // (blast radius, affected tests, flow trace) can surface corrupted
        // symbol names. One batched query per traversal.
        if !result.nodes.is_empty() {
            let uids: Vec<&str> = result.nodes.iter().map(|node| node.uid.as_str()).collect();
            match self.batch_lookup_symbols(&uids) {
                Ok(map) => {
                    for node in &mut result.nodes {
                        if let Some(sym) = map.get(&node.uid) {
                            node.name = sym.name.clone();
                            node.file_path = sym.file_path.clone();
                            node.start_line = sym.start_line;
                        }
                    }
                }
                // Non-fatal: the traversal result is still valid, display
                // fields just keep their scan-provided values.
                Err(e) => {
                    tracing::warn!("impact: display-field repair lookup failed: {e}");
                }
            }
        }
        Ok(result)
    }

    /// Internal: fetch all direct callers of `uid` across
    /// CALLS/IMPORTS/EXTENDS_SYM/IMPLEMENTS_SYM/INCLUDES_SYM/CROSS_REPO_LINK.
    /// Like [`direct_callers_of`], but reuses a caller-supplied connection and
    /// prepared statements instead of creating a connection and re-preparing
    /// one statement per edge type on EVERY call.
    ///
    /// `impact_bfs` calls this once per visited node, so this setup is paid
    /// once per traversal instead of once per node (nw-065).
    fn direct_callers_prepared(
        conn: &lbug::Connection<'_>,
        stmts: &mut [(String, lbug::PreparedStatement)],
        uid: &str,
        min_confidence: f32,
        edges: &[EdgeType],
    ) -> Vec<CallerRow> {
        let min_conf = min_confidence as f64;
        let mut rows: Vec<CallerRow> = Vec::new();
        for (edge_label, stmt) in stmts.iter_mut() {
            // Only follow the edge types active at this depth.
            if !edges
                .iter()
                .any(|e| e.rel_table_name() == edge_label.as_str())
            {
                continue;
            }
            let result = match conn.execute(
                stmt,
                vec![
                    ("uid", lbug::Value::String(uid.to_string())),
                    ("min_conf", lbug::Value::Double(min_conf)),
                ],
            ) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!("impact: edge type {edge_label} query failed: {e}");
                    continue;
                }
            };
            for row in result {
                use lbug::Value;
                let caller_uid = match &row[0] {
                    // caller_uid is a scan-projected string and can be garbled
                    // by the engine's partial-scan corruption (#678). It is the
                    // NAVIGATIONAL key — following a garbled uid would add a
                    // phantom node the PK repair can never resolve — so skip a
                    // corrupt row rather than walk into garbage. name/file_path
                    // are re-resolved from PK lookups after the walk regardless.
                    Value::String(s) if crate::read::string_is_corrupt(s) => continue,
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
                    edge_type: edge_label.clone(),
                    confidence,
                });
            }
        }
        rows
    }

    /// Prepare one caller-lookup statement per edge type, for reuse across a
    /// whole traversal. Edge types whose relationship table does not exist are
    /// skipped (same tolerance as the per-call path).
    fn prepare_caller_stmts(
        conn: &lbug::Connection<'_>,
        edges: &[EdgeType],
    ) -> Vec<(String, lbug::PreparedStatement)> {
        let mut out = Vec::new();
        for edge_type in edges.iter().map(|e| e.rel_table_name()) {
            let q = format!(
                "MATCH (s:Symbol)-[r:{et}]->(t:Symbol {{uid: $uid}}) \
                 WHERE r.confidence >= $min_conf \
                 RETURN s.uid, s.name, s.file_path, s.start_line, r.confidence",
                et = edge_type,
            );
            match conn.prepare(&q) {
                Ok(stmt) => out.push((edge_type.to_string(), stmt)),
                Err(e) => {
                    tracing::trace!(
                        "impact: edge type {edge_type} skipped (table may not exist): {e}"
                    );
                }
            }
        }
        out
    }

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

    /// Snapshot the generation and consult the cache while serialized with
    /// dirty-marker transitions. The publication barrier is always acquired
    /// before `symbol_name_cache`, matching cache invalidation's lock order.
    fn begin_symbol_name_cache_query(&self) -> (u64, Option<std::sync::Arc<SymbolNameCached>>) {
        let _publication = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let generation = self.graph_generation();
        if self.is_index_publication_dirty() {
            return (generation, None);
        }

        let cached = self
            .symbol_name_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|entry| entry.generation == generation)
            .map(std::sync::Arc::clone);
        (generation, cached)
    }

    /// Publish a completed full-table query only if the graph is still the
    /// same clean generation that the query began against. Rechecking dirty
    /// state and generation while holding the publication barrier makes the
    /// check-and-fill atomic with marker establishment/retirement.
    #[cfg(test)]
    fn finalize_symbol_name_cache_query(
        &self,
        query_generation: u64,
        symbols: Vec<(String, nestweaver_schema::Symbol)>,
    ) -> std::sync::Arc<SymbolNameCached> {
        self.finalize_symbol_name_cache_query_with_hook(query_generation, symbols, || {})
    }

    fn finalize_symbol_name_cache_query_with_hook(
        &self,
        query_generation: u64,
        symbols: Vec<(String, nestweaver_schema::Symbol)>,
        inside_publication_barrier: impl FnOnce(),
    ) -> std::sync::Arc<SymbolNameCached> {
        let candidate = std::sync::Arc::new(SymbolNameCached {
            generation: query_generation,
            symbols,
        });
        let _publication = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        inside_publication_barrier();
        let current_generation = self.graph_generation();
        if self.is_index_publication_dirty() || current_generation != query_generation {
            return candidate;
        }

        let mut cached = self
            .symbol_name_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = cached
            .as_ref()
            .filter(|entry| entry.generation == current_generation)
        {
            return std::sync::Arc::clone(entry);
        }

        *cached = Some(std::sync::Arc::clone(&candidate));
        candidate
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
        Ok(self
            .search_symbols_by_name_page(query, limit, seed_resolution)?
            .symbols)
    }

    /// Counted symbol-name search. Result retention is bounded by `limit`, and
    /// total precision is bounded by [`SYMBOL_SEARCH_COUNT_CAP`].
    pub fn search_symbols_by_name_page(
        &self,
        query: &str,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
    ) -> Result<SymbolSearchPage, StoreError> {
        self.search_symbols_by_name_page_impl(
            query,
            limit,
            seed_resolution,
            SYMBOL_SEARCH_COUNT_CAP,
            || {},
            || {},
        )
    }

    /// Test-only entry to the exact public-search implementation with
    /// deterministic synchronization points around cache publication.
    #[cfg(test)]
    fn search_symbols_by_name_with_hooks(
        &self,
        query: &str,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
        after_db_scan: impl FnOnce(),
        inside_publication_barrier: impl FnOnce(),
    ) -> Result<Vec<nestweaver_schema::Symbol>, StoreError> {
        Ok(self
            .search_symbols_by_name_page_impl(
                query,
                limit,
                seed_resolution,
                SYMBOL_SEARCH_COUNT_CAP,
                after_db_scan,
                inside_publication_barrier,
            )?
            .symbols)
    }

    #[cfg(test)]
    fn search_symbols_by_name_page_with_hooks(
        &self,
        query: &str,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
        count_cap: usize,
        after_db_scan: impl FnOnce(),
        inside_publication_barrier: impl FnOnce(),
    ) -> Result<SymbolSearchPage, StoreError> {
        self.search_symbols_by_name_page_impl(
            query,
            limit,
            seed_resolution,
            count_cap,
            after_db_scan,
            inside_publication_barrier,
        )
    }

    fn search_symbols_by_name_page_impl(
        &self,
        query: &str,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
        count_cap: usize,
        after_db_scan: impl FnOnce(),
        inside_publication_barrier: impl FnOnce(),
    ) -> Result<SymbolSearchPage, StoreError> {
        if limit > crate::tantivy_index::SEARCH_PRESENTATION_LIMIT_MAX {
            return Err(StoreError::PresentationLimitExceeded {
                limit,
                max: crate::tantivy_index::SEARCH_PRESENTATION_LIMIT_MAX,
            });
        }
        let needle = query.to_lowercase();
        // --- Step 1: coherently snapshot publication state + cache ----------
        // On a clean hit we clone the Arc (cheap ref-count bump). Dirty
        // publications always bypass hits. The DB query itself remains outside
        // the publication barrier so an index writer is never blocked by a
        // full-table scan.
        let (query_generation, cached_symbols) = self.begin_symbol_name_cache_query();

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

            after_db_scan();
            self.finalize_symbol_name_cache_query_with_hook(
                query_generation,
                all,
                inside_publication_barrier,
            )
        };

        // --- Step 3: filter and rank the in-memory list ----------------------
        // Collect all substring matches, score by name quality × path factor,
        // then take the top `limit` by descending adjusted score with
        // kind-priority + file-path tiebreaks. This prevents test/playwright
        // files from dominating when a PascalCase name also appears in
        // production code, and gives a deterministic order across calls.
        let mut candidates = BinaryHeap::new();
        let mut counted = 0usize;
        let mut count_saturated = false;
        for (ordinal, (lower, sym)) in entry.symbols.iter().enumerate() {
            if !lower.contains(&needle) {
                continue;
            }
            if counted < count_cap {
                counted += 1;
                if counted == count_cap {
                    count_saturated = true;
                }
            } else {
                count_saturated = true;
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
            if limit == 0 {
                continue;
            }
            let ranked = RankedSymbol {
                adjusted_score: adjusted,
                kind_rank: kind_rank(sym.kind, &seed_resolution.kind_priority),
                file_path: &sym.file_path,
                ordinal,
                symbol: sym,
            };
            if candidates.len() < limit {
                candidates.push(ranked);
            } else if candidates.peek().is_some_and(|worst| ranked < *worst) {
                candidates.pop();
                candidates.push(ranked);
            }
        }
        let mut candidates = candidates.into_vec();
        candidates.sort();
        let symbols = candidates
            .into_iter()
            .map(|candidate| candidate.symbol.clone())
            .collect();
        let total = if count_saturated {
            SearchTotal::lower_bound(counted)
        } else {
            SearchTotal::exact(counted)
        };
        Ok(SymbolSearchPage { symbols, total })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier};

    use nestweaver_schema::{Symbol, SymbolKind, Visibility};

    use super::{compute_path_factor, kind_rank};
    use crate::db::GraphStore;
    use crate::ranking::{PathDeboostRule, SeedResolutionConfig, default_kind_priority};
    use crate::tantivy_index::SearchTotalRelation;

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

    /// nw-111: the callee traversal must REPORT which edge reached each callee.
    ///
    /// It spans CALLS, IMPORTS and CROSS_REPO_LINK, and the last is an inferred
    /// link between repos rather than an observed call. Collapsing them to a bare
    /// symbol list meant `flow_trace` presented fabricated cross-language
    /// execution paths as real ones — tracing a Rust function returned JavaScript
    /// symbols from unrelated repos as its callees, with nothing in the payload
    /// to tell them apart. `impact` has always returned the edge type; this is the
    /// callee-side parity.
    #[test]
    fn callee_traversal_reports_the_edge_type_that_reached_each_callee() {
        use nestweaver_schema::{CrossRepoLinkType, EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["root", "real_callee", "remote_guess"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }

        // An observed call...
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "root".to_string(),
                target_uid: "real_callee".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        // ...and an INFERRED cross-repo link, which is not a call at all.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "root".to_string(),
                target_uid: "remote_guess".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: Vec::new(),
            })
            .unwrap();

        let callees = store.callees_with_edge_types_of("root").unwrap();
        let by_uid: std::collections::HashMap<&str, &str> = callees
            .iter()
            .map(|(sym, et)| (sym.uid.as_str(), et.as_str()))
            .collect();

        assert_eq!(
            by_uid.get("real_callee"),
            Some(&"CALLS"),
            "an observed call must be labelled CALLS; got {by_uid:?}"
        );
        assert_eq!(
            by_uid.get("remote_guess"),
            Some(&"CROSS_REPO_LINK"),
            "a cross-repo hypothesis must be labelled, not presented as a call; got {by_uid:?}"
        );

        // The unlabelled helper still returns both, so existing callers are
        // unaffected — this adds information rather than removing any.
        let plain = store.callees_of("root").unwrap();
        assert_eq!(plain.len(), 2, "callees_of must be unchanged in coverage");
    }

    /// A callee reachable by BOTH a real call and a cross-repo link must be
    /// reported as the stronger CALLS, not downgraded to a guess.
    #[test]
    fn a_callee_reachable_two_ways_reports_the_strongest_edge() {
        use nestweaver_schema::{CrossRepoLinkType, EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["root", "shared"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "root".to_string(),
                target_uid: "shared".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: Vec::new(),
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "root".to_string(),
                target_uid: "shared".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let callees = store.callees_with_edge_types_of("root").unwrap();
        assert_eq!(callees.len(), 1, "de-duplicated by symbol: {callees:?}");
        assert_eq!(
            callees[0].1, "CALLS",
            "strongest evidence wins: {callees:?}"
        );
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

    #[test]
    fn search_symbols_by_name_page_counts_before_the_presentation_limit() {
        let store = GraphStore::in_memory().unwrap();
        for (uid, name) in [
            ("s1", "Payment"),
            ("s2", "PaymentGateway"),
            ("s3", "RetryPayment"),
        ] {
            store.insert_symbol(&make_symbol(uid, name)).unwrap();
        }

        let page = store
            .search_symbols_by_name_page("payment", 1, &no_rules())
            .unwrap();
        assert_eq!(page.symbols.len(), 1);
        assert_eq!(page.total.value, 3);
        assert_eq!(page.total.relation, SearchTotalRelation::Exact);

        let legacy = store
            .search_symbols_by_name("payment", 1, &no_rules())
            .unwrap();
        assert_eq!(page.symbols[0].uid, legacy[0].uid);
    }

    #[test]
    fn search_symbols_by_name_page_reports_lower_bound_without_changing_top_hits() {
        let store = GraphStore::in_memory().unwrap();
        for (uid, name) in [
            ("s1", "Payment"),
            ("s2", "PaymentGateway"),
            ("s3", "RetryPayment"),
        ] {
            store.insert_symbol(&make_symbol(uid, name)).unwrap();
        }

        let page = store
            .search_symbols_by_name_page_with_hooks("payment", 1, &no_rules(), 2, || {}, || {})
            .unwrap();
        assert_eq!(page.symbols.len(), 1);
        assert_eq!(page.symbols[0].uid, "s1");
        assert_eq!(page.total.value, 2);
        assert_eq!(page.total.relation, SearchTotalRelation::LowerBound);
    }

    #[test]
    fn search_symbols_by_name_page_zero_limit_does_not_materialize_results() {
        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&make_symbol("s1", "Payment")).unwrap();

        let page = store
            .search_symbols_by_name_page("payment", 0, &no_rules())
            .unwrap();
        assert!(page.symbols.is_empty());
        assert_eq!(page.total.value, 1);
        assert_eq!(page.total.relation, SearchTotalRelation::Exact);
    }

    #[test]
    fn symbol_search_entry_points_reject_extreme_presentation_limits() {
        use crate::tantivy_index::SEARCH_PRESENTATION_LIMIT_MAX;

        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&make_symbol("s1", "Payment")).unwrap();

        assert!(
            store
                .search_symbols_by_name("payment", 0, &no_rules())
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .search_symbols_by_name("payment", SEARCH_PRESENTATION_LIMIT_MAX, &no_rules())
                .is_ok()
        );
        assert!(
            store
                .search_symbols_by_name_page("payment", SEARCH_PRESENTATION_LIMIT_MAX, &no_rules(),)
                .is_ok()
        );

        for over_limit in [SEARCH_PRESENTATION_LIMIT_MAX + 1, usize::MAX] {
            assert!(matches!(
                store.search_symbols_by_name("payment", over_limit, &no_rules()),
                Err(crate::StoreError::PresentationLimitExceeded { .. })
            ));
            assert!(
                store
                    .search_symbols_by_name_page("payment", over_limit, &no_rules())
                    .is_err()
            );
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

    #[test]
    fn symbol_name_cache_bypasses_hits_and_fills_while_publication_is_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = std::path::PathBuf::from(format!("{}.index-dirty", db_path.display()));
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store
            .insert_symbol(&make_symbol("fresh", "NeedleFresh"))
            .unwrap();

        let publication = store.acquire_index_publication_lease().unwrap();
        store.with_index_publication_rank_barrier(|| {
            std::fs::write(&marker_path, b"dirty").unwrap();
            publication.reserve_generation().unwrap();
        });

        let poisoned = Arc::new(super::SymbolNameCached {
            generation: store.graph_generation(),
            symbols: vec![("needlestale".into(), make_symbol("stale", "NeedleStale"))],
        });
        *store.symbol_name_cache.lock().unwrap() = Some(Arc::clone(&poisoned));

        let results = store
            .search_symbols_by_name("needle", 10, &no_rules())
            .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|symbol| symbol.uid.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh"],
            "dirty readers must query the database instead of using a matching cache entry"
        );
        let cached = store.symbol_name_cache.lock().unwrap();
        assert!(
            Arc::ptr_eq(cached.as_ref().unwrap(), &poisoned),
            "a dirty-window query must not replace or refill the cache"
        );
    }

    #[test]
    fn symbol_name_cache_bypasses_cache_after_dirty_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = std::path::PathBuf::from(format!("{}.index-dirty", db_path.display()));
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store
                .insert_symbol(&make_symbol("fresh", "ReopenFresh"))
                .unwrap();
            store.bump_and_persist_generation();
        }
        std::fs::write(&marker_path, b"dirty").unwrap();

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let poisoned = Arc::new(super::SymbolNameCached {
            generation: reopened.graph_generation(),
            symbols: vec![("reopenstale".into(), make_symbol("stale", "ReopenStale"))],
        });
        *reopened.symbol_name_cache.lock().unwrap() = Some(Arc::clone(&poisoned));

        let results = reopened
            .search_symbols_by_name("reopen", 10, &no_rules())
            .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|symbol| symbol.uid.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh"]
        );
        assert!(Arc::ptr_eq(
            reopened.symbol_name_cache.lock().unwrap().as_ref().unwrap(),
            &poisoned
        ));
    }

    #[test]
    fn symbol_name_cache_real_query_does_not_fill_when_publication_turns_dirty_after_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = std::path::PathBuf::from(format!("{}.index-dirty", db_path.display()));
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        store
            .insert_symbol(&make_symbol("fresh", "RaceFresh"))
            .unwrap();
        let query_generation = store.graph_generation();
        assert!(store.symbol_name_cache.lock().unwrap().is_none());
        let scan_finished = Arc::new(Barrier::new(2));
        let release_reader = Arc::new(Barrier::new(2));

        let reader = {
            let store = Arc::clone(&store);
            let scan_finished = Arc::clone(&scan_finished);
            let release_reader = Arc::clone(&release_reader);
            std::thread::spawn(move || {
                store.search_symbols_by_name_with_hooks(
                    "race",
                    10,
                    &no_rules(),
                    || {
                        scan_finished.wait();
                        release_reader.wait();
                    },
                    || {},
                )
            })
        };

        scan_finished.wait();
        let publication = store.acquire_index_publication_lease().unwrap();
        store.with_index_publication_rank_barrier(|| {
            std::fs::write(&marker_path, b"dirty").unwrap();
            publication.reserve_generation().unwrap();
        });
        assert_ne!(
            store.graph_generation(),
            query_generation,
            "the paused query must have captured the preceding clean generation"
        );
        assert!(store.symbol_name_cache.lock().unwrap().is_none());

        release_reader.wait();
        let results = reader.join().unwrap().unwrap();

        assert_eq!(
            results
                .iter()
                .map(|symbol| symbol.uid.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh"],
            "the dirty-window reader still returns its uncached DB result"
        );
        assert!(
            store.symbol_name_cache.lock().unwrap().is_none(),
            "the reader must not publish after the writer's completed invalidation"
        );
    }

    #[test]
    fn symbol_name_cache_does_not_fill_when_generation_changes_during_fetch() {
        let store = GraphStore::in_memory().unwrap();
        let query_generation = store.graph_generation();
        store.bump_graph_generation();

        store.finalize_symbol_name_cache_query(
            query_generation,
            vec![("old".into(), make_symbol("old", "Old"))],
        );

        assert!(
            store.symbol_name_cache.lock().unwrap().is_none(),
            "a query result from an older generation must not be published"
        );
    }

    #[test]
    fn symbol_name_cache_dirty_query_cannot_fill_after_clean_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = std::path::PathBuf::from(format!("{}.index-dirty", db_path.display()));
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.insert_symbol(&make_symbol("new", "New")).unwrap();
        let publication = store.acquire_index_publication_lease().unwrap();
        store.with_index_publication_rank_barrier(|| {
            std::fs::write(&marker_path, b"dirty").unwrap();
            publication.reserve_generation().unwrap();
        });
        let dirty_generation = store.graph_generation();

        store.with_index_publication_rank_barrier(|| {
            publication.publish_clean_generation().unwrap();
            std::fs::remove_file(&marker_path).unwrap();
            publication.complete_generation().unwrap();
        });
        publication.release().unwrap();

        store.finalize_symbol_name_cache_query(
            dirty_generation,
            vec![("old".into(), make_symbol("old", "Old"))],
        );
        assert!(store.symbol_name_cache.lock().unwrap().is_none());

        let results = store
            .search_symbols_by_name("new", 10, &no_rules())
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].uid, "new");
        assert_eq!(
            store
                .symbol_name_cache
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .generation,
            store.graph_generation(),
            "the first query after clean publication must fill only the final generation"
        );
    }

    #[test]
    fn symbol_name_cache_final_check_and_fill_hold_publication_barrier() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("fresh", "BarrierFresh"))
            .unwrap();
        let results = store
            .search_symbols_by_name_with_hooks(
                "barrier",
                10,
                &no_rules(),
                || {},
                || {
                    assert!(
                        matches!(
                            store.pagerank_compute_lock.try_lock(),
                            Err(std::sync::TryLockError::WouldBlock)
                        ),
                        "final generation/dirty validation and fill must own the publication barrier"
                    );
                },
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].uid, "fresh");
        assert_eq!(
            store
                .symbol_name_cache
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .generation,
            store.graph_generation()
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

    /// The opt-out: `impact_with_flags_and_threshold` with `0.0` must return the
    /// full traversal — a depth-4 chain of 0.5-confidence edges (score 0.0625)
    /// that the default threshold prunes — and must not set
    /// `truncated_by_threshold`.
    #[test]
    fn impact_with_flags_and_threshold_zero_disables_pruning() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["target", "a", "b", "c", "d"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // d → c → b → a → target, every edge at 0.5 confidence:
        // d's decayed score is 0.5^4 = 0.0625 < DEFAULT_IMPACT_THRESHOLD (0.10).
        for (src, tgt) in [("a", "target"), ("b", "a"), ("c", "b"), ("d", "c")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: tgt.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.5,
                    link_type: None,
                    evidence: Vec::new(),
                })
                .unwrap();
        }

        // Default threshold: d is pruned and the honesty flag fires.
        let pruned = store.impact_with_flags("target", 10, 0.0, None).unwrap();
        assert!(
            pruned.truncated_by_threshold,
            "the default threshold must prune the 0.0625-score depth-4 caller"
        );
        assert!(
            !pruned.nodes.iter().any(|n| n.uid == "d"),
            "d must be pruned under the default threshold"
        );

        // Opt-out: threshold 0.0 returns the full traversal with no prune flag.
        let full = store
            .impact_with_flags_and_threshold("target", 10, 0.0, 0.0, None)
            .unwrap();
        assert!(
            !full.truncated_by_threshold,
            "threshold 0.0 disables score pruning, so nothing is pruned"
        );
        let d = full
            .nodes
            .iter()
            .find(|n| n.uid == "d")
            .expect("the real depth-4 caller must be included when pruning is off");
        assert_eq!(d.depth, 4);
        assert!(
            (d.impact_score - 0.0625).abs() < 1e-9,
            "d keeps its true decayed score, got {}",
            d.impact_score
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

    // ── data-dependence edge tier ───────────────────────────────────────

    /// The data tier surfaces symbols that only reference the changed *type*
    /// (`Uses`) or read/write its *field* (`Accesses`) — dependents the
    /// structural-only walk misses entirely.
    #[test]
    fn impact_with_data_edges_follows_type_and_field_edges() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["changed", "caller", "reader"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // caller references the changed type; reader accesses a changed field.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "caller".to_string(),
                target_uid: "changed".to_string(),
                edge_type: EdgeType::Uses,
                confidence: 0.9,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "reader".to_string(),
                target_uid: "changed".to_string(),
                edge_type: EdgeType::Accesses,
                confidence: 0.9,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        // Data tier on: both the type-reference and field-access dependents show.
        let with_data = store
            .impact_with_data_edges("changed", 3, 0.0, 2, None)
            .unwrap();
        let data_uids: Vec<&str> = with_data.nodes.iter().map(|n| n.uid.as_str()).collect();
        assert!(
            data_uids.contains(&"caller"),
            "Uses (type-reference) dependent must surface with the data tier; got: {data_uids:?}"
        );
        assert!(
            data_uids.contains(&"reader"),
            "Accesses (field-access) dependent must surface with the data tier; got: {data_uids:?}"
        );

        // Structural-only: neither is reachable (default-off behavior).
        let structural = store.impact_with_flags("changed", 3, 0.0, None).unwrap();
        let struct_uids: Vec<&str> = structural.nodes.iter().map(|n| n.uid.as_str()).collect();
        assert!(
            !struct_uids.contains(&"caller"),
            "structural-only walk must NOT follow Uses edges; got: {struct_uids:?}"
        );
        assert!(
            !struct_uids.contains(&"reader"),
            "structural-only walk must NOT follow Accesses edges; got: {struct_uids:?}"
        );
    }

    /// Data edges are shallow-capped: a `Uses` chain deeper than
    /// `data_max_depth` is not followed past the cap, while the structural set
    /// still traverses to full `max_depth`.
    #[test]
    fn data_edges_are_depth_capped() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["changed", "d1", "d2", "s1", "s2", "s3"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // A Uses chain: d2 --Uses--> d1 --Uses--> changed.
        for (src, tgt) in [("d1", "changed"), ("d2", "d1")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: tgt.to_string(),
                    edge_type: EdgeType::Uses,
                    confidence: 0.9,
                    link_type: None,
                    evidence: Vec::new(),
                })
                .unwrap();
        }
        // A structural Calls chain: s3 -> s2 -> s1 -> changed.
        for (src, tgt) in [("s1", "changed"), ("s2", "s1"), ("s3", "s2")] {
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

        // data_max_depth = 1: data edges are followed only from the seed (depth
        // 0), so d1 (depth 1) is reached but d2 (depth 2) is not.
        let result = store
            .impact_with_data_edges("changed", 5, 0.0, 1, None)
            .unwrap();
        let uids: Vec<&str> = result.nodes.iter().map(|n| n.uid.as_str()).collect();
        assert!(
            uids.contains(&"d1"),
            "the first Uses hop is within the data cap; got: {uids:?}"
        );
        assert!(
            !uids.contains(&"d2"),
            "a Uses hop past data_max_depth must NOT be followed; got: {uids:?}"
        );
        // Structural edges still traverse to full max_depth.
        for s in ["s1", "s2", "s3"] {
            assert!(
                uids.contains(&s),
                "structural chain must traverse to full depth; missing {s}, got: {uids:?}"
            );
        }
    }

    /// A reverse-adjacency snapshot is not eligible to replace the live
    /// traversal unless a known-nonempty mixed graph produces the exact same
    /// ordered payload, including score bits and honesty flags.
    #[test]
    fn impact_snapshot_is_byte_equivalent_for_a_known_nonempty_graph() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in [
            "target",
            "slow",
            "fast",
            "diamond",
            "downstream",
            "reader",
            "deep-reader",
            "pruned",
        ] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }

        let edge =
            |source: &str, target: &str, edge_type: EdgeType, confidence: f32| ResolvedEdge {
                source_uid: source.to_string(),
                target_uid: target.to_string(),
                edge_type,
                confidence,
                link_type: None,
                evidence: Vec::new(),
            };
        for resolved in [
            edge("slow", "target", EdgeType::Calls, 0.4),
            edge("fast", "target", EdgeType::Imports, 0.9),
            edge("diamond", "slow", EdgeType::Calls, 0.95),
            edge("diamond", "fast", EdgeType::Extends, 0.6),
            edge("downstream", "diamond", EdgeType::Calls, 0.8),
            // Close a cycle back to the seed. The seed must never appear in
            // its own impact result.
            edge("target", "downstream", EdgeType::Calls, 0.7),
            edge("reader", "target", EdgeType::Uses, 0.85),
            // data_max_depth=1 must keep this second data hop out.
            edge("deep-reader", "reader", EdgeType::Uses, 0.9),
            // The default cumulative threshold must prune this caller.
            edge("pruned", "target", EdgeType::Calls, 0.05),
        ] {
            store.insert_edge(&resolved).unwrap();
        }

        let live = store
            .impact_bfs_live(
                "target",
                4,
                0.0,
                super::IMPACT_EDGE_TYPES,
                super::IMPACT_DATA_EDGE_TYPES,
                1,
                super::DEFAULT_IMPACT_THRESHOLD,
                None,
                None,
            )
            .unwrap();
        let live_uids: Vec<&str> = live.nodes.iter().map(|node| node.uid.as_str()).collect();
        assert_eq!(
            live_uids,
            vec!["fast", "reader", "diamond", "downstream", "slow"],
            "the equivalence fixture must stay nonempty and exercise the better diamond path"
        );
        assert!(live.truncated_by_threshold);
        assert!(!live.truncated_by_depth);
        let diamond = live
            .nodes
            .iter()
            .find(|node| node.uid == "diamond")
            .unwrap();
        assert_eq!(
            diamond.impact_score.to_bits(),
            (0.9_f32 as f64 * 0.6_f32 as f64).to_bits(),
            "the maximum-confidence path through fast must win"
        );

        let from_snapshot = store
            .impact_with_data_edges("target", 4, 0.0, 1, None)
            .unwrap();

        let as_bytes = |result: &super::ImpactResult| {
            let nodes: Vec<_> = result
                .nodes
                .iter()
                .map(|node| {
                    serde_json::json!([
                        node.uid,
                        node.name,
                        node.file_path,
                        node.start_line,
                        node.edge_type,
                        node.confidence.to_bits(),
                        node.depth,
                        node.impact_score.to_bits(),
                    ])
                })
                .collect();
            let edge_types: Vec<_> = result
                .edge_types
                .iter()
                .map(|edge| edge.rel_table_name())
                .collect();
            serde_json::to_vec(&serde_json::json!({
                "nodes": nodes,
                "truncated_by_threshold": result.truncated_by_threshold,
                "truncated_by_depth": result.truncated_by_depth,
                "edge_types": edge_types,
            }))
            .unwrap()
        };
        assert_eq!(
            as_bytes(&from_snapshot),
            as_bytes(&live),
            "the snapshot traversal must be byte-equivalent before it can replace the live path"
        );
    }

    #[test]
    fn impact_snapshot_cache_reuses_one_arc_within_a_generation() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["caller", "target"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "caller".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.95,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let first = store
            .cached_impact_snapshot(None)
            .unwrap()
            .expect("a clean generation must be cacheable");
        let second = store
            .cached_impact_snapshot(None)
            .unwrap()
            .expect("the clean cache must remain available");

        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated impact queries in one generation must reuse the exact snapshot"
        );
    }

    #[test]
    fn impact_snapshot_cache_invalidates_on_generation_change() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["first", "second", "target"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "first".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.95,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        let before = store
            .cached_impact_snapshot(None)
            .unwrap()
            .expect("the initial generation must be cacheable");

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "second".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        store.bump_graph_generation();

        let after = store
            .cached_impact_snapshot(None)
            .unwrap()
            .expect("the new clean generation must be cacheable");
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a generation change must replace, never reuse, the prior snapshot"
        );
        let result = after
            .impact_with_data_edges("target", 1, 0.0, 0, None)
            .unwrap();
        assert_eq!(
            result
                .nodes
                .iter()
                .map(|node| node.uid.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["first", "second"]),
            "the replacement snapshot must include graph changes from the new generation"
        );
    }

    #[test]
    fn impact_snapshot_cache_discards_a_candidate_when_generation_changes_during_load() {
        let store = GraphStore::in_memory().unwrap();

        let result = store
            .cached_impact_snapshot_with_loader(None, || {
                let candidate = store.load_impact_snapshot()?;
                store.bump_graph_generation();
                Ok(candidate)
            })
            .unwrap();

        assert!(
            result.is_none(),
            "a candidate built across a generation change must not be served"
        );
        assert!(
            store.impact_snapshot_cache.lock().unwrap().is_none(),
            "a raced candidate must not be published"
        );
    }

    #[test]
    fn impact_snapshot_cache_single_flights_concurrent_first_touch() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let store = Arc::new(GraphStore::in_memory().unwrap());
        let start = Arc::new(Barrier::new(9));
        let loads = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();

        for _ in 0..8 {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            let loads = Arc::clone(&loads);
            workers.push(std::thread::spawn(move || {
                start.wait();
                store
                    .cached_impact_snapshot_with_loader(None, || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        store.load_impact_snapshot()
                    })
                    .unwrap()
                    .expect("a clean generation must publish a snapshot")
            }));
        }

        start.wait();
        let snapshots: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();

        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "concurrent first-touch queries must perform one snapshot construction"
        );
        assert!(
            snapshots
                .iter()
                .skip(1)
                .all(|snapshot| Arc::ptr_eq(&snapshots[0], snapshot)),
            "every waiter must receive the one published snapshot"
        );
    }

    #[test]
    fn impact_snapshot_single_flight_wait_is_cancellable() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        let store = Arc::new(GraphStore::in_memory().unwrap());
        let load_started = Arc::new(Barrier::new(2));
        let release_loader = Arc::new(Barrier::new(2));
        let builder = {
            let store = Arc::clone(&store);
            let load_started = Arc::clone(&load_started);
            let release_loader = Arc::clone(&release_loader);
            std::thread::spawn(move || {
                store
                    .cached_impact_snapshot_with_loader(None, || {
                        load_started.wait();
                        release_loader.wait();
                        store.load_impact_snapshot()
                    })
                    .unwrap()
            })
        };
        load_started.wait();

        let cancel = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = mpsc::channel();
        let waiter = {
            let store = Arc::clone(&store);
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                let result = store.cached_impact_snapshot_with_loader(Some(&cancel), || {
                    panic!("a single-flight waiter must never invoke a second loader")
                });
                result_tx
                    .send(result.map(|snapshot| snapshot.is_some()))
                    .unwrap();
            })
        };
        std::thread::sleep(Duration::from_millis(25));
        cancel.store(true, Ordering::Relaxed);
        let result_before_release = result_rx.recv_timeout(Duration::from_millis(250));

        release_loader.wait();
        builder.join().unwrap();
        waiter.join().unwrap();

        assert!(
            matches!(
                result_before_release,
                Ok(Err(crate::error::StoreError::Cancelled(
                    crate::error::CancelReason::Timeout
                )))
            ),
            "a cancelled waiter must return before the active loader is released; got \
             {result_before_release:?}"
        );
    }

    #[test]
    fn impact_query_populates_the_generation_keyed_snapshot_cache() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["caller", "target"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "caller".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.95,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        assert!(store.impact_snapshot_cache.lock().unwrap().is_none());

        let first = store.impact("target", 1, 0.0).unwrap();
        let cached = store
            .impact_snapshot_cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, snapshot)| Arc::clone(snapshot))
            .expect("the first clean impact query must publish its snapshot");
        let second = store.impact("target", 1, 0.0).unwrap();
        let reused = store
            .impact_snapshot_cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, snapshot)| Arc::clone(snapshot))
            .expect("the cached snapshot must remain published");

        assert_eq!(
            first
                .iter()
                .map(|node| node.uid.as_str())
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|node| node.uid.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            Arc::ptr_eq(&cached, &reused),
            "the second impact query must reuse the first query's snapshot"
        );
    }

    #[test]
    fn impact_query_falls_back_to_live_when_snapshot_construction_fails() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        let mut caller = make_symbol("caller", "caller");
        caller.signature = "fn caller()\0".to_string();
        store.insert_symbol(&caller).unwrap();
        store
            .insert_symbol(&make_symbol("target", "target"))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "caller".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.95,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        assert!(
            store.load_impact_snapshot().is_err(),
            "the malformed full-symbol projection must exercise the cache failure path"
        );
        let result = store.impact("target", 1, 0.0).unwrap();

        assert_eq!(
            result
                .iter()
                .map(|node| node.uid.as_str())
                .collect::<Vec<_>>(),
            vec!["caller"],
            "cache construction failure must preserve live traversal results"
        );
        assert!(
            store.impact_snapshot_cache.lock().unwrap().is_none(),
            "a failed snapshot must not be published"
        );
    }

    #[test]
    fn impact_query_never_serves_cached_adjacency_while_publication_is_dirty() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = std::path::PathBuf::from(format!("{}.index-dirty", db_path.display()));
        let store = GraphStore::open_or_create(&db_path).unwrap();
        for uid in ["first", "second", "target"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "first".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.95,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        store.impact("target", 1, 0.0).unwrap();
        let old_snapshot = store
            .impact_snapshot_cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, snapshot)| Arc::clone(snapshot))
            .unwrap();

        let publication = store.acquire_index_publication_lease().unwrap();
        store.with_index_publication_rank_barrier(|| {
            std::fs::write(&marker_path, b"dirty").unwrap();
            publication.reserve_generation().unwrap();
        });
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "second".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let dirty_result = store.impact("target", 1, 0.0).unwrap();
        assert_eq!(
            dirty_result
                .iter()
                .map(|node| node.uid.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["first", "second"]),
            "a dirty-window query must use the live graph, not stale cached adjacency"
        );
        assert!(
            store.impact_snapshot_cache.lock().unwrap().is_none(),
            "publication invalidation must release the old snapshot before graph writes"
        );

        store.with_index_publication_rank_barrier(|| {
            publication.publish_clean_generation().unwrap();
            std::fs::remove_file(&marker_path).unwrap();
            publication.complete_generation().unwrap();
        });
        publication.release().unwrap();

        store.impact("target", 1, 0.0).unwrap();
        let current_snapshot = store
            .impact_snapshot_cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, snapshot)| Arc::clone(snapshot))
            .unwrap();
        assert!(
            !Arc::ptr_eq(&old_snapshot, &current_snapshot),
            "the first clean query after publication must replace the old generation"
        );
    }

    #[test]
    fn cancelled_impact_snapshot_load_publishes_nothing() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let store = GraphStore::in_memory().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let error = store
            .cached_impact_snapshot_with_loader(Some(&cancel), || {
                cancel.store(true, Ordering::Relaxed);
                store.load_impact_snapshot()
            })
            .expect_err("a cancelled first load must fail");

        assert!(matches!(
            error,
            crate::error::StoreError::Cancelled(crate::error::CancelReason::Timeout)
        ));
        assert!(
            store.impact_snapshot_cache.lock().unwrap().is_none(),
            "a cancelled candidate must never enter the cache"
        );
    }

    const BULK_ENDPOINT_SYMBOL_COUNT: usize = 2_048;

    /// Build a chain graph of `BULK_ENDPOINT_SYMBOL_COUNT` symbols joined by
    /// `Calls` edges, shared by the bulk-load correctness and performance tests.
    fn bulk_endpoint_store(uid_prefix: &str) -> (GraphStore, Vec<nestweaver_schema::Symbol>) {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        let symbols: Vec<_> = (0..BULK_ENDPOINT_SYMBOL_COUNT)
            .map(|index| {
                let uid = format!("{uid_prefix}-{index:04}");
                make_symbol(&uid, &uid)
            })
            .collect();
        store.batch_insert_symbols(&symbols).unwrap();

        let edges: Vec<_> = symbols
            .windows(2)
            .map(|pair| ResolvedEdge {
                source_uid: pair[0].uid.clone(),
                target_uid: pair[1].uid.clone(),
                edge_type: EdgeType::Calls,
                confidence: 0.95,
                link_type: None,
                evidence: Vec::new(),
            })
            .collect();
        store.batch_insert_edges(&edges).unwrap();
        (store, symbols)
    }

    /// Correctness: a bulk endpoint load must return every symbol and the
    /// complete reverse adjacency, however long the machine takes to do it.
    ///
    /// This carries no wall-clock bound on purpose. The timing budget that used
    /// to live here asserted throughput, not correctness, and failed on a loaded
    /// machine for reasons this test does not name; it now lives in
    /// `impact_snapshot_bulk_load_avoids_per_uid_queries` below. The structural
    /// assertions here are stronger than the single length check they replace.
    #[test]
    fn impact_snapshot_bulk_loads_thousands_of_endpoints() {
        use nestweaver_schema::EdgeType;

        let (store, symbols) = bulk_endpoint_store("bulk-endpoint");
        let snapshot = store.load_impact_snapshot().unwrap();

        assert_eq!(snapshot.symbols_by_uid.len(), BULK_ENDPOINT_SYMBOL_COUNT);
        for symbol in &symbols {
            assert!(
                snapshot.symbols_by_uid.contains_key(&symbol.uid),
                "bulk load dropped endpoint symbol {}",
                symbol.uid
            );
        }

        // Every symbol but the first head of the chain is called exactly once.
        let calls_label = EdgeType::Calls.rel_table_name();
        assert_eq!(
            snapshot.callers_by_target.len(),
            BULK_ENDPOINT_SYMBOL_COUNT - 1
        );
        for pair in symbols.windows(2) {
            let by_edge = snapshot
                .callers_by_target
                .get(&pair[1].uid)
                .unwrap_or_else(|| panic!("no callers recorded for {}", pair[1].uid));
            let callers = by_edge
                .get(calls_label)
                .unwrap_or_else(|| panic!("no {calls_label} callers for {}", pair[1].uid));
            assert_eq!(
                callers.len(),
                1,
                "unexpected caller fan-in on {}",
                pair[1].uid
            );
            assert_eq!(callers[0].uid, pair[0].uid);
            // Confidence round-trips through an f32 column, so compare at f32
            // precision rather than f64::EPSILON.
            assert!(
                (callers[0].confidence - 0.95).abs() < 1e-6,
                "confidence on {} -> {} round-tripped to {}",
                pair[0].uid,
                pair[1].uid,
                callers[0].confidence
            );
        }
        assert!(
            !snapshot.callers_by_target.contains_key(&symbols[0].uid),
            "the head of the chain must have no callers"
        );
    }

    /// Performance guard: the bulk load must stay one batched query, not one
    /// query per UID.
    ///
    /// Ignored by default because it is a wall-clock measurement and this
    /// repository's test suite routinely runs on machines with several
    /// concurrent builds, where an honest O(1)-query implementation still
    /// exceeds any budget sized near its idle cost. Run deliberately on an idle
    /// machine with `cargo test -p nestweaver-store -- --ignored`. The budget is
    /// deliberately far above the idle cost (~0.2s) so that it discriminates a
    /// genuine N+1 regression — which would issue 2048 separate queries and take
    /// orders of magnitude longer — rather than measuring scheduler noise.
    /// Correctness is covered unconditionally by the test above.
    #[test]
    #[ignore = "wall-clock performance guard; run deliberately on an idle machine with --ignored"]
    fn impact_snapshot_bulk_load_avoids_per_uid_queries() {
        use std::time::{Duration, Instant};

        let (store, _symbols) = bulk_endpoint_store("bulk-perf");

        let started = Instant::now();
        let snapshot = store.load_impact_snapshot().unwrap();
        let elapsed = started.elapsed();

        assert_eq!(snapshot.symbols_by_uid.len(), BULK_ENDPOINT_SYMBOL_COUNT);
        assert!(
            elapsed < Duration::from_secs(20),
            "loading {BULK_ENDPOINT_SYMBOL_COUNT} endpoint symbols took {elapsed:?}; \
             the snapshot must not issue one query per UID"
        );
    }

    #[test]
    fn cached_impact_traversal_is_subsecond_for_thousands_of_nodes() {
        use std::time::{Duration, Instant};

        use nestweaver_schema::{EdgeType, ResolvedEdge};

        const SYMBOL_COUNT: usize = 2_048;

        let store = GraphStore::in_memory().unwrap();
        let symbols: Vec<_> = (0..SYMBOL_COUNT)
            .map(|index| {
                let uid = format!("cached-impact-{index:04}");
                make_symbol(&uid, &uid)
            })
            .collect();
        store.batch_insert_symbols(&symbols).unwrap();
        let edges: Vec<_> = symbols
            .windows(2)
            .map(|pair| ResolvedEdge {
                source_uid: pair[0].uid.clone(),
                target_uid: pair[1].uid.clone(),
                edge_type: EdgeType::Calls,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .collect();
        store.batch_insert_edges(&edges).unwrap();
        let target = symbols.last().unwrap().uid.as_str();

        let warm = store
            .impact_with_flags_and_threshold(target, SYMBOL_COUNT as u32, 0.0, 0.0, None)
            .unwrap();
        assert_eq!(warm.nodes.len(), SYMBOL_COUNT - 1);

        let started = Instant::now();
        let cached = store
            .impact_with_flags_and_threshold(target, SYMBOL_COUNT as u32, 0.0, 0.0, None)
            .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(cached.nodes.len(), SYMBOL_COUNT - 1);
        assert!(
            elapsed < Duration::from_secs(1),
            "cached traversal across {SYMBOL_COUNT} nodes took {elapsed:?}"
        );
    }

    #[test]
    fn impact_snapshot_rejects_out_of_range_confidence() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["caller", "target"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "caller".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 1.5,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let error = store
            .load_impact_snapshot()
            .expect_err("invalid confidence must fail the whole snapshot");
        assert!(
            error.to_string().contains("invalid confidence 1.5"),
            "the failure must identify the invalid confidence, got: {error}"
        );
    }

    #[test]
    fn impact_snapshot_rejects_null_confidence() {
        let store = GraphStore::in_memory().unwrap();
        for uid in ["caller", "target"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        let conn = store.conn().unwrap();
        conn.query(
            "MATCH (source:Symbol {uid: 'caller'}), (target:Symbol {uid: 'target'}) \
             CREATE (source)-[:CALLS {confidence: NULL, evidence: ''}]->(target)",
        )
        .unwrap();
        drop(conn);

        let error = store
            .load_impact_snapshot()
            .expect_err("missing confidence must fail the whole snapshot");
        assert!(
            error.to_string().contains("confidence"),
            "the failure must identify the missing confidence, got: {error}"
        );
    }

    #[test]
    fn impact_snapshot_rejects_missing_impact_edge_table() {
        let store = GraphStore::in_memory().unwrap();
        let conn = store.conn().unwrap();
        conn.query("DROP TABLE CALLS").unwrap();
        drop(conn);

        let error = store
            .load_impact_snapshot()
            .expect_err("a missing impact edge table must fail the whole snapshot");
        assert!(
            error.to_string().contains("CALLS"),
            "the failure must identify the missing edge table, got: {error}"
        );
    }

    // ── cycle termination ───────────────────────────────────────────────

    /// A cyclic call graph (A→B→A) must not send the reverse-BFS into an
    /// infinite loop. The "scores only increase" invariant means a node is
    /// re-enqueued only when a strictly better path is found; since each hop
    /// multiplies by a confidence ≤ 1.0 the score cannot keep improving around
    /// a cycle, so the walk terminates with a finite node set.
    #[test]
    fn impact_terminates_on_two_node_cycle() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["A", "B"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // A calls B and B calls A — a 2-cycle over impact-relevant edges.
        for (src, tgt) in [("A", "B"), ("B", "A")] {
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

        // impact(A): the only caller of A is B (seed A is skipped when reached
        // again around the cycle). Terminates with exactly {B}.
        let nodes = store.impact("A", 10, 0.0).unwrap();
        let uids: Vec<&str> = nodes.iter().map(|n| n.uid.as_str()).collect();
        assert_eq!(uids, vec!["B"], "2-cycle must yield exactly the caller set");

        // impact_detailed returns the same finite set without hanging.
        let result = store
            .impact_detailed("A", 10, 0.0, IMPACT_EDGE_TYPES, None)
            .unwrap();
        let detailed_uids: Vec<&str> = result.nodes.iter().map(|n| n.uid.as_str()).collect();
        assert_eq!(detailed_uids, vec!["B"]);
    }

    /// A three-node cycle (A→B→C→A) also terminates, yielding the finite set of
    /// transitive callers reachable before the walk loops back to the seed.
    #[test]
    fn impact_terminates_on_three_node_cycle() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        for uid in ["A", "B", "C"] {
            store.insert_symbol(&make_symbol(uid, uid)).unwrap();
        }
        // A→B→C→A.
        for (src, tgt) in [("A", "B"), ("B", "C"), ("C", "A")] {
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

        // impact(A): caller of A is C, caller of C is B, caller of B is A (seed,
        // skipped). Terminates with {B, C}.
        let nodes = store.impact("A", 10, 0.0).unwrap();
        let mut uids: Vec<&str> = nodes.iter().map(|n| n.uid.as_str()).collect();
        uids.sort_unstable();
        assert_eq!(
            uids,
            vec!["B", "C"],
            "3-cycle must yield the finite caller set"
        );
    }
}
