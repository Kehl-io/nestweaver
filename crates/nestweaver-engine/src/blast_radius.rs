// PR blast radius analysis: maps changed files to affected symbols,
// runs transitive impact analysis, groups by cluster, and scores risk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::{GraphStore, ImpactNode, StoreError};

use crate::cluster_dispatch::{ClusteringOutput, load_clusters};
use crate::process::RiskLevel;

/// A symbol that was directly changed (lives in a changed file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedSymbol {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub kind: String,
    pub pagerank_score: Option<f64>,
    /// The repo_uid that owns this symbol (from the graph store).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo_uid: String,
}

/// A symbol transitively affected by a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedSymbol {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub kind: String,
    pub depth: u32,
    pub edge_type: String,
    pub confidence: f32,
    /// 1-based start line of the symbol in its file, from the impact node.
    /// Lets consumers (e.g. SARIF regions) anchor at the real location rather
    /// than defaulting to line 1.
    #[serde(default)]
    pub start_line: u32,
    /// Confidence-weighted impact score (1.0 = direct high-confidence edge,
    /// decays multiplicatively through the graph). Used for sorting results
    /// so the most-affected symbols appear first.
    pub impact_score: f64,
    /// The repo_uid that owns this symbol (from the graph store).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo_uid: String,
    /// Whether `repo_uid` came from an authoritative symbol lookup. Kept out
    /// of serialized output so deserialized/legacy rows default to unknown and
    /// cannot assert their own authorization provenance.
    #[serde(skip)]
    pub ownership_resolved: bool,
}

/// A cluster (community) that contains affected symbols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedCluster {
    pub id: u32,
    pub name: String,
    pub affected_count: usize,
    pub total_count: usize,
    pub cohesion: f64,
}

/// Org-wide impact from an upstream server (if available).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgWideImpact {
    /// Breaking impact items from the upstream.
    pub breaking: Vec<OrgImpactItem>,
    /// Warning impact items from the upstream.
    pub warnings: Vec<OrgImpactItem>,
    /// Info-level impact items from the upstream.
    pub info: Vec<OrgImpactItem>,
    /// Repos impacted across the org.
    pub impacted_repos: Vec<String>,
    /// Name of the upstream server.
    pub source_server: String,
}

/// A single org-wide impact item (from the upstream server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgImpactItem {
    pub change_name: String,
    pub change_kind: String,
    /// Stable repo identity for the changed/source symbol. Authorization must
    /// use this field rather than a display label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub change_repo_uid: String,
    pub affected_name: String,
    /// Stable repo identity for the affected/destination symbol. The
    /// `affected_repo` field below is presentation-only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub affected_repo_uid: String,
    pub affected_repo: String,
    pub affected_file: String,
    pub affected_line: i32,
    pub severity: String,
    pub reason: String,
}

/// Whether the analysis ran to completion. Ordered by severity so status can
/// only escalate (Complete < Partial < Degraded < Failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnalysisStatus {
    // Old serialized results (no `status` field) predate the trust core and
    // described healthy runs, so they deserialize as Complete.
    #[default]
    Complete,
    Partial,
    Degraded,
    Failed,
}

impl AnalysisStatus {
    /// Lowercase label for embedding in the human summary string.
    ///
    /// Public so CLI text renderers can surface the status verbatim rather than
    /// re-deriving the mapping (nw-107).
    pub fn label(self) -> &'static str {
        match self {
            AnalysisStatus::Complete => "complete",
            AnalysisStatus::Partial => "partial",
            AnalysisStatus::Degraded => "degraded",
            AnalysisStatus::Failed => "failed",
        }
    }
}

/// Severity of a [`Notification`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    Note,
    Warning,
    Error,
}

/// A machine-readable reason the analysis was incomplete or degraded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub level: NotificationLevel,
    pub message: String,
    /// Stable kebab/dotted reason code, e.g. "store.impact-failed".
    pub descriptor: String,
}

/// The gate verdict. Derived, never over-approximated from a degraded run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateState {
    Ok,
    // Absent gate state on an old result maps to the conservative "unknown"
    // rather than falsely asserting Ok.
    #[default]
    DegradedUnknown,
    RiskFlagged,
}

/// A repo that is indexed but behind its source (its graph may be out of date).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleRepo {
    pub repo_uid: String,
    pub commits_behind: u32,
}

/// Which repos were actually covered by this analysis, and how completely. Lets
/// a consumer tell "no impact" from "incomplete coverage": a repo referenced by
/// the change but absent from the graph, or a stale/truncated traversal, means
/// the reported impact is a floor, not the whole picture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Coverage {
    /// Distinct repos that owned a changed or affected symbol.
    pub repos_in_scope: Vec<String>,
    /// Repos referenced by the change but not present in the graph (not indexed).
    pub repos_not_indexed: Vec<String>,
    /// In-scope repos whose index is behind source (`commits_behind > 0`).
    pub stale_repos: Vec<StaleRepo>,
    /// Whether any impact traversal was cut short by depth or the score
    /// threshold — true means real dependents may exist beyond the reported set.
    pub traversal_truncated: bool,
}

/// A category of impact that static analysis cannot see. The first four are
/// inherent gaps in any static call-graph traversal; the last three describe
/// this specific run being cut short — by the score threshold or the depth cap —
/// or missing an indexed repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlindSpot {
    DynamicDispatch,
    Reflection,
    ConfigWiring,
    Codegen,
    /// The walk dropped dependents whose confidence fell below the score
    /// threshold — real but low-signal dependents may exist beyond the set.
    PrunedBelowThreshold,
    /// The walk hit the depth cap before exhausting the call graph — dependents
    /// deeper than `max_depth` are not represented. Distinct from threshold
    /// pruning: widening depth (not lowering the threshold) is what recovers them.
    DepthTruncated,
    NotIndexed,
}

/// Full result of a blast radius analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlastRadiusResult {
    pub changed_symbols: Vec<ChangedSymbol>,
    pub affected_symbols: Vec<AffectedSymbol>,
    /// Total affected symbols before presentation limits, within the result's
    /// current visibility scope.
    #[serde(default)]
    pub affected_symbol_count: usize,
    pub affected_clusters: Vec<AffectedCluster>,
    pub risk_level: RiskLevel,
    pub summary: String,
    /// Org-wide impacts from the upstream server (if available).
    /// `None` when no upstream is configured or the server is unreachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_wide: Option<OrgWideImpact>,
    /// Whether the analysis ran to completion. A degraded/failed run must NOT
    /// be read as "safe" — see `gate_state`.
    #[serde(default)]
    pub status: AnalysisStatus,
    /// Machine-readable reasons the analysis was incomplete or degraded.
    #[serde(default)]
    pub notifications: Vec<Notification>,
    /// The gate verdict, derived from `status` + `risk_level`. Never emits
    /// `RiskFlagged` from a degraded run (that would be an over-approximation);
    /// a degraded run is `DegradedUnknown` so a consumer treats it as unknown.
    #[serde(default)]
    pub gate_state: GateState,
    /// Which repos were in scope, stale, or not indexed, and whether the
    /// traversal was truncated — so "no impact" is distinguishable from
    /// "incomplete coverage".
    #[serde(default)]
    pub coverage: Coverage,
    /// Categories of impact this static analysis cannot see.
    #[serde(default)]
    pub blind_spots: Vec<BlindSpot>,
    /// Historically co-changing files for the changed set (advisory tier —
    /// not part of risk scoring; empty when the cochange sidecar is absent,
    /// disclosed via a `cochange-unavailable` note).
    #[serde(default)]
    pub cochanged_files: Vec<CoChangedFile>,
    /// How to read the result: static impact analysis over-approximates edges
    /// (it may report reachable-but-not-actually-affected symbols) while
    /// under-approximating the blind spots above.
    #[serde(default)]
    pub analysis_direction: String,
}

/// A file historically coupled to a changed file via git co-change mining
/// (Jaccard confidence), with no requirement of a static edge. Advisory:
/// closes the logical-coupling blind spot (serializer/config/cross-language
/// pairs) that the static graph cannot see.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangedFile {
    /// The coupled file (the one NOT in the changed set).
    pub file: String,
    /// The changed file it historically ships with.
    pub coupled_to: String,
    /// Commits in which both changed together.
    pub cochange_count: u32,
    /// Jaccard coefficient of the pairing.
    pub confidence: f32,
    /// Human-readable framing.
    pub note: String,
}

/// Derive the gate verdict from the run status and computed risk.
///
/// NON-NEGOTIABLE rule: a run that did not complete is never `RiskFlagged`
/// (we cannot trust an incomplete traversal to have found the risk), it is
/// `DegradedUnknown`.
pub(crate) fn derive_gate_state(status: AnalysisStatus, risk_level: RiskLevel) -> GateState {
    if status != AnalysisStatus::Complete {
        GateState::DegradedUnknown
    } else if matches!(risk_level, RiskLevel::High) {
        GateState::RiskFlagged
    } else {
        GateState::Ok
    }
}

/// Render the human summary line from result counts. Shared between the analysis
/// path and the R9b redaction path so a redacted result's summary is regenerated
/// from its (redacted) vecs and can never echo pre-redaction, cross-repo counts.
pub(crate) fn render_blast_summary(
    changed_symbols: usize,
    changed_files: usize,
    affected_symbols: usize,
    clusters_touched: usize,
    risk_level: RiskLevel,
    status: AnalysisStatus,
) -> String {
    let mut summary = format!(
        "{changed_symbols} changed symbol(s) in {changed_files} file(s), \
         {affected_symbols} transitively affected symbol(s), \
         {clusters_touched} cluster(s) touched. Risk: {risk_level:?}."
    );
    // Make a non-clean run impossible to miss in the human summary.
    if status != AnalysisStatus::Complete {
        summary.push_str(&format!(" [status: {}]", status.label()));
    }
    summary
}

/// Apply the presentation-only cap after analysis and any authorization
/// redaction have established the result's visible total.
pub fn apply_affected_symbol_limit(result: &mut BlastRadiusResult, limit: Option<usize>) {
    if let Some(n) = limit
        && result.affected_symbols.len() > n
    {
        let total = result.affected_symbol_count;
        result.affected_symbols.truncate(n);
        result.notifications.push(Notification {
            level: NotificationLevel::Note,
            message: format!(
                "affected symbols truncated to {n} of {total} (raise `limit` for the full set)"
            ),
            descriptor: "results-truncated".to_string(),
        });
    }
}

/// Depth to which data-dependence edges (type references and field access)
/// are followed when `include_data_edges` is on. Shallow because these edges
/// fan out toward full program slices if followed transitively.
const DATA_EDGE_MAX_DEPTH: u32 = 2;

/// Tunable inputs for [`analyze_blast_radius`]. Grouped into a struct so the
/// call sites stay readable as knobs accrete.
#[derive(Debug, Clone)]
pub struct BlastRadiusOptions {
    /// Repo under review; scopes changed-file resolution in a unified
    /// multi-repo graph. `None` matches identical relative paths across repos.
    pub target_repo: Option<String>,
    /// Maximum traversal depth for the impact walk.
    pub max_depth: u32,
    /// Also follow shallow data-dependence edges (type refs & field access).
    pub include_data_edges: bool,
    /// Cap on returned `affected_symbols` (most-impactful first). None = no cap.
    pub limit: Option<usize>,
}

impl Default for BlastRadiusOptions {
    fn default() -> Self {
        Self {
            target_repo: None,
            // A derived `Default` would give `max_depth: 0` — i.e. no traversal
            // at all, a silent footgun. 3 matches the CLI/MCP default depth.
            max_depth: 3,
            include_data_edges: false,
            limit: None,
        }
    }
}

/// Analyze the blast radius of a set of changed files.
///
/// 1. Maps changed files to their symbols in the graph.
/// 2. For each symbol, runs transitive impact analysis over the structural
///    edges (CALLS, IMPORTS, EXTENDS, IMPLEMENTS, INCLUDES, CROSS_REPO_LINK)
///    up to `max_depth`. When `include_data_edges` is on, the shallow
///    data-dependence tier (USES, ACCESSES) is also followed.
/// 3. Groups affected symbols by cluster/community (if cluster data exists).
/// 4. Scores risk based on: number of affected symbols, PageRank centrality
///    of changed symbols, and number of clusters touched.
///
/// Risk levels (`RiskLevel` has three variants):
/// - Low: <10 affected symbols
/// - Medium: 10-50 affected symbols
/// - High: 50+ affected symbols (everything >200 also maps to High)
///
/// Centrality and cluster boosts can escalate a level (see `compute_risk_level`).
pub fn analyze_blast_radius(
    store: &GraphStore,
    changed_files: &[PathBuf],
    options: &BlastRadiusOptions,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    db_path: Option<&Path>,
) -> Result<BlastRadiusResult> {
    let target_repo = options.target_repo.as_deref();
    let max_depth = options.max_depth;
    let include_data_edges = options.include_data_edges;

    // Trust core: track whether the analysis actually ran to completion and why
    // not. A failed/partial query must NOT be reported as "nothing affected".
    let mut status = AnalysisStatus::Complete;
    let mut notifications: Vec<Notification> = Vec::new();

    // Resolve repo_uid -> display name (repo URL when available, else the uid)
    // for org-wide impact reporting. Fetched up front so the changed-file loop
    // can tell "unindexed repo" from "indexed repo, drifted path".
    let repos = match store.list_repos(None) {
        Ok(r) => r,
        Err(e) => {
            notifications.push(Notification {
                level: NotificationLevel::Error,
                message: format!("failed to list repos: {e}"),
                descriptor: "store.list-repos-failed".to_string(),
            });
            status = status.max(AnalysisStatus::Degraded);
            Vec::new()
        }
    };
    let known_repo_uids: HashSet<String> = repos.iter().map(|r| r.uid.clone()).collect();
    // Retain staleness so an in-scope repo whose index is behind source is
    // surfaced in coverage rather than silently trusted as up to date.
    let repo_staleness: HashMap<String, u32> = repos
        .iter()
        .map(|r| (r.uid.clone(), r.staleness_commits_behind))
        .collect();
    let repo_display: HashMap<String, String> = repos
        .into_iter()
        .map(|r| (r.uid.clone(), if r.url.is_empty() { r.uid } else { r.url }))
        .collect();

    // Step 1: Map changed files to symbols.
    let mut changed_symbols: Vec<ChangedSymbol> = Vec::new();
    let mut changed_uids: HashSet<String> = HashSet::new();
    // Repos that own the changed symbols — used to separate same-repo ("local")
    // impact from cross-repo ("org-wide") impact below.
    let mut changed_repos: HashSet<String> = HashSet::new();
    // Count file lookups that succeeded vs errored, so a wholesale store failure
    // (every lookup errored) escalates to a hard `Failed` rather than a quiet
    // "0 symbols".
    let mut files_ok: usize = 0;
    let mut files_errored: usize = 0;

    for file in changed_files {
        let file_str = file.to_string_lossy();
        // In a unified multi-repo graph, scope resolution to the repo under
        // review so identical relative paths don't conflate across repos.
        let lookup = match target_repo {
            Some(repo) => store.symbols_in_file_in_repo(&file_str, repo),
            None => store.symbols_in_file(&file_str),
        };
        let syms = match lookup {
            Ok(syms) => {
                files_ok += 1;
                syms
            }
            Err(e) => {
                files_errored += 1;
                notifications.push(Notification {
                    level: NotificationLevel::Error,
                    message: format!("failed to resolve symbols for {file_str}: {e}"),
                    descriptor: "store.symbols-lookup-failed".to_string(),
                });
                status = status.max(AnalysisStatus::Degraded);
                continue;
            }
        };

        // A successful lookup that resolves 0 symbols is suspicious for any
        // recognized source file: it may be new, the index may be stale, or the
        // path may have drifted. Non-source files legitimately have no symbols.
        if syms.is_empty() {
            if nestweaver_parser::detect_language(file).is_some() {
                notifications.push(Notification {
                    level: NotificationLevel::Warning,
                    message: format!(
                        "changed source file {file_str} has no indexed symbols (new file, stale \
                         index, or path drift) — its impact was not assessed"
                    ),
                    descriptor: "changed-file-no-symbols".to_string(),
                });
                status = status.max(AnalysisStatus::Partial);
            }
            continue;
        }

        for sym in syms {
            changed_repos.insert(sym.repo_uid.clone());
            if changed_uids.insert(sym.uid.clone()) {
                changed_symbols.push(ChangedSymbol {
                    uid: sym.uid.clone(),
                    name: sym.name.clone(),
                    file_path: sym.file_path.clone(),
                    kind: sym.kind.to_string(),
                    pagerank_score: sym.pagerank_score,
                    repo_uid: sym.repo_uid.clone(),
                });
            }
        }
    }

    // nw-059: symbol rows never carry PageRank (it lives in the ranking
    // cache/sidecar), so hydrate changed-symbol scores from the cache — else
    // the centrality risk boost can never fire on a production DB. `ranks_len`
    // (the graph size the scores normalize over) feeds the relative
    // high-centrality threshold in Step 4; 0 means no cache was available.
    let mut ranks_len: usize = 0;
    if !changed_symbols.is_empty() {
        store.ensure_pagerank_loaded();
        let ranks = store.pagerank_scores();
        if !ranks.is_empty() {
            ranks_len = ranks.len();
            for cs in &mut changed_symbols {
                if let Some(r) = ranks.get(&cs.uid) {
                    cs.pagerank_score = Some(*r);
                }
            }
        }
    }

    // Hard failure: we had files to analyze and *every* lookup errored. The
    // store is broken, not merely partial — a consumer must not read this as
    // "nothing affected".
    if !changed_files.is_empty() && files_ok == 0 && files_errored > 0 {
        status = status.max(AnalysisStatus::Failed);
    }

    // Empty-index guard: we resolved no changed symbols AND the database has no
    // repositories indexed — the index is missing/empty (e.g. a wrong or unbuilt
    // --db), so this is "cannot assess", NOT "nothing affected". Surface it
    // loudly rather than returning a confident clean result. Gated on
    // `files_errored == 0` so a store failure keeps its Degraded/Failed status
    // and its own notification instead of this one.
    if changed_symbols.is_empty()
        && known_repo_uids.is_empty()
        && files_errored == 0
        && !changed_files.is_empty()
    {
        notifications.push(Notification {
            level: NotificationLevel::Warning,
            message: "no repositories are indexed in this database — cannot assess blast \
                      radius (is the index built, and is --db pointing at it?)"
                .to_string(),
            descriptor: "index-empty".to_string(),
        });
        status = status.max(AnalysisStatus::Degraded);
    }

    // Step 2: For each changed symbol, run transitive impact analysis.
    let mut affected_symbols: Vec<AffectedSymbol> = Vec::new();
    let mut affected_uids: HashSet<String> = HashSet::new();
    // Cross-repo (org-wide) impact items, bucketed by severity, plus the set of
    // downstream repos touched.
    let mut org_breaking: Vec<OrgImpactItem> = Vec::new();
    let mut org_warnings: Vec<OrgImpactItem> = Vec::new();
    let mut org_info: Vec<OrgImpactItem> = Vec::new();
    let mut impacted_repos: HashSet<String> = HashSet::new();

    // Affected nodes whose per-symbol lookup (kind/owning-repo) errored. We
    // fall back to empty kind/repo but count them so we can surface ONE
    // aggregated note instead of spamming one per node.
    let mut lookup_failures: usize = 0;

    // OR of every traversal's truncation flags. True means at least one impact
    // walk was cut short (by depth or the score threshold), so the reported
    // affected set is a floor, not the complete picture.
    // Tracked separately so a depth-capped run and a threshold-pruned run get
    // the right blind-spot label (they have different remedies).
    let mut truncated_by_threshold = false;
    let mut truncated_by_depth = false;

    // Emit the cancellation notification + degrade once. Set when the timeout
    // trips so the summary/gate reflect an incomplete run.
    let push_cancelled = |notifications: &mut Vec<Notification>, status: &mut AnalysisStatus| {
        notifications.push(Notification {
            level: NotificationLevel::Warning,
            message: "impact analysis cancelled (timeout) before completing".to_string(),
            descriptor: "analysis-cancelled".to_string(),
        });
        *status = (*status).max(AnalysisStatus::Degraded);
    };

    // First pass buffers each newly-seen affected node together with the
    // changed symbol that surfaced it (name/kind), so we can batch the per-node
    // kind/owning-repo lookup into ONE store query afterwards instead of N.
    // Buffering in first-seen order (the same order the old per-node path pushed
    // into `affected_symbols`) keeps the pre-sort ordering byte-identical.
    let mut buffered: Vec<(ImpactNode, String, String, String)> = Vec::new();

    for cs in &changed_symbols {
        // Check the deadline before starting another symbol's traversal so a
        // tripped timeout stops promptly rather than after the whole set.
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            push_cancelled(&mut notifications, &mut status);
            break;
        }

        // Optionally fold in the shallow data-dependence tier (type references
        // & field access). Default off: higher recall but noisier.
        let impact_call = if include_data_edges {
            store.impact_with_data_edges(&cs.uid, max_depth, 0.0, DATA_EDGE_MAX_DEPTH, cancel)
        } else {
            store.impact_with_flags(&cs.uid, max_depth, 0.0, cancel)
        };
        let impact_nodes = match impact_call {
            Ok(result) => {
                truncated_by_threshold |= result.truncated_by_threshold;
                truncated_by_depth |= result.truncated_by_depth;
                result.nodes
            }
            // A cancelled traversal means the run is incomplete — the timeout
            // fired mid-walk. Stop processing further symbols; the reported
            // blast radius is a floor, not the whole picture.
            Err(StoreError::Cancelled(_)) => {
                push_cancelled(&mut notifications, &mut status);
                break;
            }
            Err(e) => {
                // Do NOT silently drop this symbol's downstream as if empty —
                // that would under-report the blast radius.
                notifications.push(Notification {
                    level: NotificationLevel::Error,
                    message: format!("impact traversal failed for {}: {e}", cs.name),
                    descriptor: "store.impact-failed".to_string(),
                });
                status = status.max(AnalysisStatus::Degraded);
                continue;
            }
        };
        for node in impact_nodes {
            // Skip symbols that are themselves in the changed set.
            if changed_uids.contains(&node.uid) {
                continue;
            }
            // Dedup: first-seen wins for a uid's node data. Because impact scores
            // only increase along the traversal, the first traversal to reach a
            // node already carries its best data — same semantics as the prior
            // `if affected_uids.insert(uid) { ... }` per-node path. Buffer the
            // node plus its surfacing changed symbol; the kind/repo lookup is
            // deferred to a single batched query below.
            if affected_uids.insert(node.uid.clone()) {
                buffered.push((node, cs.name.clone(), cs.kind.clone(), cs.repo_uid.clone()));
            }
        }
    }

    // Batch the per-node kind/owning-repo enrichment into ONE store query,
    // replacing the former N per-affected-node `lookup_symbol` round-trips.
    let uid_refs: Vec<&str> = buffered
        .iter()
        .map(|(node, _, _, _)| node.uid.as_str())
        .collect();
    let lookup_map = match store.batch_lookup_symbols(&uid_refs) {
        Ok(map) => map,
        // Loud failure: a store error on the batch lookup is surfaced and
        // degrades the run (like the impact-traversal error path). We then treat
        // every node's kind/repo as unknown/empty rather than crashing — the
        // aggregated `lookup-symbol-failed` note below still fires for the misses.
        Err(e) => {
            notifications.push(Notification {
                level: NotificationLevel::Error,
                message: format!("batch symbol lookup failed: {e}"),
                descriptor: "store.batch-lookup-failed".to_string(),
            });
            status = status.max(AnalysisStatus::Degraded);
            std::collections::HashMap::new()
        }
    };

    // Second pass: rebuild affected_symbols (and the org-wide items) from the
    // buffered nodes + the batch lookup map, in the same first-seen order, so the
    // pre-sort output is identical to the old per-node path.
    for (node, change_name, change_kind, change_repo_uid) in buffered {
        let affected_sym = lookup_map.get(&node.uid);
        if affected_sym.is_none() {
            lookup_failures += 1;
        }
        let kind = affected_sym.map(|s| s.kind.to_string()).unwrap_or_default();
        let affected_repo = affected_sym.map(|s| s.repo_uid.clone()).unwrap_or_default();

        // If the affected symbol lives in a different repo than any
        // changed symbol, it is a cross-repo (org-wide) impact.
        if !affected_repo.is_empty() && !changed_repos.contains(&affected_repo) {
            impacted_repos.insert(affected_repo.clone());
            let repo_label = repo_display
                .get(&affected_repo)
                .cloned()
                .unwrap_or_else(|| affected_repo.clone());
            let item = OrgImpactItem {
                change_name,
                change_kind,
                change_repo_uid,
                affected_name: node.name.clone(),
                affected_repo_uid: affected_repo.clone(),
                affected_repo: repo_label,
                affected_file: node.file_path.clone(),
                affected_line: node.start_line as i32,
                severity: classify_org_severity(node.impact_score).to_string(),
                reason: format!(
                    "cross-repo dependency (via {}) — verify the downstream consumer \
                     still works against the changed symbol",
                    node.edge_type
                ),
            };
            match classify_org_severity(node.impact_score) {
                "breaking" => org_breaking.push(item),
                "warning" => org_warnings.push(item),
                _ => org_info.push(item),
            }
        }

        affected_symbols.push(AffectedSymbol {
            uid: node.uid,
            name: node.name,
            file_path: node.file_path,
            kind,
            depth: node.depth,
            edge_type: node.edge_type,
            confidence: node.confidence,
            start_line: node.start_line,
            impact_score: node.impact_score,
            repo_uid: affected_repo.clone(),
            ownership_resolved: affected_sym.is_some(),
        });
    }

    // One aggregated note when any affected node's kind/repo couldn't be
    // resolved — the blast radius is still reported, but enrichment is partial.
    if lookup_failures > 0 {
        notifications.push(Notification {
            level: NotificationLevel::Note,
            message: format!(
                "could not resolve kind/owning-repo for {lookup_failures} affected symbol(s)"
            ),
            descriptor: "lookup-symbol-failed".to_string(),
        });
        status = status.max(AnalysisStatus::Partial);
    }

    // Sort affected symbols by impact_score (highest first).
    affected_symbols.sort_by(|a, b| {
        b.impact_score
            .partial_cmp(&a.impact_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.uid.cmp(&b.uid))
    });

    // The verdict inputs are snapshotted BEFORE any display cap: a `limit` is
    // presentation, and a risk/gate verdict computed from a truncated set is
    // non-safe by definition (Rothermel-Harrold; audit nw-058).
    let total_affected = affected_symbols.len();

    // Step 3: Group by clusters if cluster data is available.
    let mut affected_clusters: Vec<AffectedCluster> = Vec::new();
    let all_affected_uids: HashSet<&str> = changed_uids
        .iter()
        .chain(affected_uids.iter())
        .map(|s| s.as_str())
        .collect();

    if let Some(db) = db_path {
        match load_clusters(db) {
            Ok(Some(clustering)) => {
                affected_clusters = compute_affected_clusters(&clustering, &all_affected_uids);
            }
            // No clustering computed for this graph — legitimate, not a failure.
            Ok(None) => {}
            // A cluster read error silently drops the cluster-count risk boost,
            // which would under-report risk on an otherwise "Complete" run — the
            // exact silent-degradation this analysis exists to surface.
            Err(e) => {
                notifications.push(Notification {
                    level: NotificationLevel::Warning,
                    message: format!(
                        "cluster data unavailable — cluster-based risk may be under-reported: {e}"
                    ),
                    descriptor: "load-clusters-failed".to_string(),
                });
                status = status.max(AnalysisStatus::Degraded);
            }
        }
    }

    // Step 4: Score risk (from the pre-truncation `total_affected`).
    let clusters_touched = affected_clusters.len();

    // Factor in PageRank centrality: if high-centrality symbols are changed,
    // bump the risk. Average PageRank of changed symbols.
    let avg_pagerank = if changed_symbols.is_empty() {
        0.0
    } else {
        let sum: f64 = changed_symbols
            .iter()
            .filter_map(|s| s.pagerank_score)
            .sum();
        let count = changed_symbols
            .iter()
            .filter(|s| s.pagerank_score.is_some())
            .count();
        if count > 0 { sum / count as f64 } else { 0.0 }
    };

    // High centrality: the average changed-symbol PageRank clears 10x the
    // graph mean (normalized scores sum to ~1 over ranks_len symbols, so the
    // mean is 1/N and any absolute threshold is meaningless across graph
    // sizes). Falls back to the legacy absolute 0.01 when no ranking cache
    // exists. The boost is deliberately modest — the centrality-as-risk
    // literature is positive but contested (Zimmermann 2008 vs TSE 2021).
    // Capped at 0.5: on tiny graphs 10/N exceeds any possible score (they sum
    // to ~1), but an avg holding >50% of the total rank mass is unambiguously
    // central at any size.
    let high_centrality = if ranks_len > 0 {
        avg_pagerank > (10.0 / ranks_len as f64).min(0.5)
    } else {
        avg_pagerank > 0.01
    };

    let risk_level = compute_risk_level(total_affected, clusters_touched, high_centrality);

    // nw-105: a truncated traversal must not report as Complete.
    //
    // The truncation flags are accumulated at :598, long before this point, but
    // `status` was left at Complete — so `derive_gate_state` below returned Ok
    // for a run that simultaneously reported coverage.traversal_truncated and a
    // depth-truncated blind spot. A merge gate consuming gate_state read "ok"
    // for an analysis that never finished.
    //
    // Fixed here rather than inside derive_gate_state: that function's rule is
    // already correct and stating it twice would let the two copies drift, and
    // more importantly `status` itself is consumed by the summary line, the MCP
    // envelope and pr-impact. Downgrading only from Complete so a run already
    // Degraded or Failed is never upgraded.
    if (truncated_by_threshold || truncated_by_depth) && status == AnalysisStatus::Complete {
        status = AnalysisStatus::Partial;
    }

    let summary = render_blast_summary(
        changed_symbols.len(),
        changed_files.len(),
        total_affected,
        clusters_touched,
        risk_level,
        status,
    );

    // Gate verdict — a degraded/failed run is never RiskFlagged (see
    // `derive_gate_state`); it is reported as unknown so consumers don't read a
    // broken analysis as safe.
    let gate_state = derive_gate_state(status, risk_level);

    // Build the org-wide (cross-repo) impact summary, if any. Sourced from this
    // daemon's unified multi-repo graph. A connected upstream server can augment
    // or override this at the client layer.
    let org_wide = if impacted_repos.is_empty() {
        None
    } else {
        let mut repos: Vec<String> = impacted_repos
            .iter()
            .map(|uid| {
                repo_display
                    .get(uid)
                    .cloned()
                    .unwrap_or_else(|| uid.clone())
            })
            .collect();
        repos.sort();
        Some(OrgWideImpact {
            breaking: org_breaking,
            warnings: org_warnings,
            info: org_info,
            impacted_repos: repos,
            source_server: "local".to_string(),
        })
    };

    // Coverage & blind spots: report which repos were in scope, which were
    // stale or not indexed, whether the traversal was truncated, and the
    // inherent static-analysis gaps — so a consumer can tell "no impact" from
    // "incomplete coverage".
    let mut scope_uids: HashSet<String> = HashSet::new();
    for cs in &changed_symbols {
        if !cs.repo_uid.is_empty() {
            scope_uids.insert(cs.repo_uid.clone());
        }
    }
    for af in &affected_symbols {
        if !af.repo_uid.is_empty() {
            scope_uids.insert(af.repo_uid.clone());
        }
    }
    let mut repos_in_scope: Vec<String> = scope_uids.iter().cloned().collect();
    repos_in_scope.sort();

    let mut stale_repos: Vec<StaleRepo> = repos_in_scope
        .iter()
        .filter_map(|uid| match repo_staleness.get(uid) {
            Some(&behind) if behind > 0 => Some(StaleRepo {
                repo_uid: uid.clone(),
                commits_behind: behind,
            }),
            _ => None,
        })
        .collect();
    stale_repos.sort_by(|a, b| a.repo_uid.cmp(&b.repo_uid));

    // Any repo referenced by the change but absent from the graph: the target
    // repo (if named) plus any owning repo of a changed/affected symbol.
    let mut not_indexed: HashSet<String> = HashSet::new();
    if let Some(repo) = target_repo
        && !repo.is_empty()
        && !known_repo_uids.contains(repo)
    {
        not_indexed.insert(repo.to_string());
    }
    for uid in &scope_uids {
        if !uid.is_empty() && !known_repo_uids.contains(uid) {
            not_indexed.insert(uid.clone());
        }
    }
    let mut repos_not_indexed: Vec<String> = not_indexed.into_iter().collect();
    repos_not_indexed.sort();

    let traversal_truncated = truncated_by_threshold || truncated_by_depth;
    let coverage = Coverage {
        repos_in_scope,
        repos_not_indexed: repos_not_indexed.clone(),
        stale_repos,
        traversal_truncated,
    };

    // Inherent static-analysis gaps are always present; the run-specific ones
    // fire only when this traversal was actually cut short or missed a repo.
    let mut blind_spots = vec![
        BlindSpot::DynamicDispatch,
        BlindSpot::Reflection,
        BlindSpot::ConfigWiring,
        BlindSpot::Codegen,
    ];
    if truncated_by_threshold {
        blind_spots.push(BlindSpot::PrunedBelowThreshold);
    }
    if truncated_by_depth {
        blind_spots.push(BlindSpot::DepthTruncated);
    }
    if !repos_not_indexed.is_empty() {
        blind_spots.push(BlindSpot::NotIndexed);
    }

    let analysis_direction = "over-approximate".to_string();

    // Advisory co-change tier (nw-034): surface historically-coupled files
    // the static graph can't see. Absence of the sidecar is disclosed but
    // never degrades the run — this tier is a recall supplement, not a gate
    // input (risk integration waits on the nw-037 measurement loop).
    let mut cochanged_files: Vec<CoChangedFile> = Vec::new();
    if let Some(db) = db_path {
        let path = crate::sidecar_path(db, ".cochange.json");
        match crate::cochange::load_cochange_sidecar(&path) {
            Some(edges) => {
                let changed_set: HashSet<&str> =
                    changed_files.iter().filter_map(|p| p.to_str()).collect();

                // Qualify by repo. The sidecar now carries every indexed repo's
                // pairs rather than only the last one written, and its paths are
                // repo-RELATIVE — so without this a `CHANGELOG.md` in one repo
                // would match a `CHANGELOG.md` in another and invent a coupling
                // that does not exist (nw-062).
                //
                // `scope_uids` are the repos the changed symbols belong to;
                // `Repo.root_path` is what the miner stamped on each pair. A repo
                // with no root_path (server-side bare clone) contributes nothing
                // here, and unattributed legacy rows match nothing — both cases
                // fall through to the `cochange-no-coverage` disclosure below
                // rather than guessing.
                let scope_roots: HashSet<String> = scope_uids
                    .iter()
                    .filter_map(|uid| store.lookup_repo(uid).ok().flatten())
                    .filter_map(|repo| repo.root_path)
                    .collect();
                let mut seen: HashSet<(String, String)> = HashSet::new();
                for e in &edges {
                    // Only pairs mined from a repo actually in scope.
                    if !e.repo.is_empty() && !scope_roots.contains(&e.repo) {
                        continue;
                    }
                    let (coupled, changed) = if changed_set.contains(e.file_a.as_str())
                        && !changed_set.contains(e.file_b.as_str())
                    {
                        (e.file_b.clone(), e.file_a.clone())
                    } else if changed_set.contains(e.file_b.as_str())
                        && !changed_set.contains(e.file_a.as_str())
                    {
                        (e.file_a.clone(), e.file_b.clone())
                    } else {
                        continue;
                    };
                    if seen.insert((coupled.clone(), changed.clone())) {
                        cochanged_files.push(CoChangedFile {
                            note: format!(
                                "historically co-changes with {changed} ({} shared commits, \
                                 Jaccard {:.2}) — no static edge required; verify it ships \
                                 with this change",
                                e.cochange_count, e.confidence
                            ),
                            file: coupled,
                            coupled_to: changed,
                            cochange_count: e.cochange_count,
                            confidence: e.confidence,
                        });
                    }
                }
                cochanged_files.sort_by(|a, b| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.file.cmp(&b.file))
                });

                // A loaded sidecar is not the same as a sidecar that covers
                // THESE files (nw-062). It is written per indexed repo and
                // blind-overwritten, so in a multi-repo database it holds one
                // repo's history — every other repo takes this branch, matches
                // nothing, and would otherwise return an empty list with no
                // disclosure at all. An empty result must not be readable as
                // "this file has no historical coupling" when the truth is
                // "no history was mined for it".
                let mined: HashSet<&str> = edges
                    .iter()
                    .filter(|e| e.repo.is_empty() || scope_roots.contains(&e.repo))
                    .flat_map(|e| [e.file_a.as_str(), e.file_b.as_str()])
                    .collect();
                let unmined: Vec<&str> = changed_set
                    .iter()
                    .copied()
                    .filter(|path| !mined.contains(path))
                    .collect();
                if !unmined.is_empty() {
                    notifications.push(Notification {
                        level: NotificationLevel::Note,
                        message: format!(
                            "{} of {} changed file(s) do not appear in the co-change sidecar, so \
                             an empty co-change result for them cannot be distinguished from \
                             unmined history — the sidecar covers the most recently indexed repo \
                             only, so files from other repos in this database are not represented",
                            unmined.len(),
                            changed_set.len()
                        ),
                        descriptor: "cochange-no-coverage".to_string(),
                    });
                }
            }
            None => {
                notifications.push(Notification {
                    level: NotificationLevel::Note,
                    message: "co-change history unavailable (no .cochange.json sidecar) — \
                              historically-coupled files are not represented"
                        .to_string(),
                    descriptor: "cochange-unavailable".to_string(),
                });
            }
        }
    }

    let mut result = BlastRadiusResult {
        changed_symbols,
        affected_symbols,
        affected_symbol_count: total_affected,
        affected_clusters,
        risk_level,
        summary,
        org_wide,
        status,
        notifications,
        gate_state,
        coverage,
        blind_spots,
        cochanged_files,
        analysis_direction,
    };
    apply_affected_symbol_limit(&mut result, options.limit);
    Ok(result)
}

/// Classify a cross-repo impact by its decayed impact score.
/// High scores (direct, high-confidence cross-repo links) are breaking;
/// weaker transitive reach is a warning, and faint reach is informational.
fn classify_org_severity(impact_score: f64) -> &'static str {
    if impact_score >= 0.5 {
        "breaking"
    } else if impact_score >= 0.25 {
        "warning"
    } else {
        "info"
    }
}

/// Compute risk level based on affected count, clusters, and centrality.
/// `high_centrality` is decided at the call site (relative to the graph-mean
/// PageRank when a ranking cache exists) so this stays a pure bucketing fn.
fn compute_risk_level(
    affected_count: usize,
    clusters_touched: usize,
    high_centrality: bool,
) -> RiskLevel {
    // Base risk from affected symbol count:
    //   <10 = Low (0), 10-50 = Medium (1), 50-200 = High (2), >200 = High (3)
    let base = match affected_count {
        0..10 => 0,
        10..50 => 1,
        50..200 => 2,
        _ => 3,
    };

    // Boost for changes touching high-centrality symbols.
    let centrality_boost = if high_centrality { 1 } else { 0 };

    // Boost for touching many clusters (>3 clusters).
    let cluster_boost = if clusters_touched > 3 { 1 } else { 0 };

    let score = base + centrality_boost + cluster_boost;

    match score {
        0 => RiskLevel::Low,
        1 => RiskLevel::Medium,
        _ => RiskLevel::High,
    }
}

/// Determine which clusters are affected by the changed + transitively affected symbols.
fn compute_affected_clusters(
    clustering: &ClusteringOutput,
    affected_uids: &HashSet<&str>,
) -> Vec<AffectedCluster> {
    let mut result = Vec::new();
    for community in &clustering.communities {
        let affected_count = community
            .members
            .iter()
            .filter(|m| affected_uids.contains(m.uid.as_str()))
            .count();
        if affected_count > 0 {
            result.push(AffectedCluster {
                id: community.id,
                name: community.name.clone(),
                affected_count,
                total_count: community.member_count,
                cohesion: community.cohesion,
            });
        }
    }
    // Sort by affected count descending.
    result.sort_by_key(|c| std::cmp::Reverse(c.affected_count));
    result
}

/// Get changed files from `git diff --name-only` in the given repo path.
///
/// When `base_ref` is provided, diffs against that ref. Otherwise diffs
/// against HEAD (showing unstaged + staged changes).
pub fn changed_files_from_git(repo_path: &Path, base_ref: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new("git");
    cmd.arg("diff").arg("--name-only");

    if let Some(base) = base_ref {
        cmd.arg(base);
    }

    let output = cmd
        .current_dir(repo_path)
        .output()
        .context("failed to run git diff --name-only")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<PathBuf> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect();

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_dispatch::CommunityInfo;

    /// Build options preserving a test's prior target_repo/max_depth/data-edge
    /// intent, with no result cap.
    fn opts(
        target_repo: Option<&str>,
        max_depth: u32,
        include_data_edges: bool,
    ) -> BlastRadiusOptions {
        BlastRadiusOptions {
            target_repo: target_repo.map(str::to_string),
            max_depth,
            include_data_edges,
            limit: None,
        }
    }

    #[test]
    fn compute_risk_level_low() {
        assert_eq!(compute_risk_level(5, 1, false), RiskLevel::Low);
    }

    #[test]
    fn compute_risk_level_medium_by_count() {
        assert_eq!(compute_risk_level(25, 1, false), RiskLevel::Medium);
    }

    #[test]
    fn compute_risk_level_high_by_count() {
        assert_eq!(compute_risk_level(100, 1, false), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_critical_by_count() {
        assert_eq!(compute_risk_level(300, 1, false), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_boosted_by_centrality() {
        // 25 affected would be Medium, but high centrality bumps it to High
        assert_eq!(compute_risk_level(25, 1, true), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_boosted_by_clusters() {
        // 25 affected would be Medium, but >3 clusters bumps it to High
        assert_eq!(compute_risk_level(25, 5, false), RiskLevel::High);
    }

    #[test]
    fn compute_affected_clusters_filters_empty() {
        let clustering = ClusteringOutput {
            resolution: 1.0,
            modularity: 0.5,
            communities: vec![
                CommunityInfo {
                    id: 0,
                    name: "cluster-0".to_string(),
                    cohesion: 0.8,
                    member_count: 2,
                    members: vec![
                        crate::cluster_dispatch::ClusterMember {
                            uid: "sym:a".to_string(),
                            name: "a".to_string(),
                            file_path: "a.rs".to_string(),
                            kind: "Function".to_string(),
                        },
                        crate::cluster_dispatch::ClusterMember {
                            uid: "sym:b".to_string(),
                            name: "b".to_string(),
                            file_path: "b.rs".to_string(),
                            kind: "Function".to_string(),
                        },
                    ],
                    key_files: vec!["a.rs".to_string()],
                },
                CommunityInfo {
                    id: 1,
                    name: "cluster-1".to_string(),
                    cohesion: 0.6,
                    member_count: 1,
                    members: vec![crate::cluster_dispatch::ClusterMember {
                        uid: "sym:c".to_string(),
                        name: "c".to_string(),
                        file_path: "c.rs".to_string(),
                        kind: "Function".to_string(),
                    }],
                    key_files: vec!["c.rs".to_string()],
                },
            ],
        };

        let affected: HashSet<&str> = ["sym:a"].into_iter().collect();
        let clusters = compute_affected_clusters(&clustering, &affected);

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].id, 0);
        assert_eq!(clusters[0].affected_count, 1);
        assert_eq!(clusters[0].total_count, 2);
    }

    #[test]
    fn analyze_blast_radius_empty_store() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("nonexistent.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();
        assert!(result.changed_symbols.is_empty());
        assert!(result.affected_symbols.is_empty());
        assert_eq!(result.risk_level, RiskLevel::Low);
        // An empty/missing index with changes to assess must NOT read as a
        // confident "nothing affected" — it is a degraded, unknown result.
        assert_eq!(result.status, AnalysisStatus::Degraded);
        assert_eq!(result.gate_state, GateState::DegradedUnknown);
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "index-empty"),
            "empty index must surface an index-empty notification: {:?}",
            result.notifications
        );
        assert!(result.notifications.iter().any(|n| {
            n.level == NotificationLevel::Warning
                && n.descriptor == "changed-file-no-symbols"
                && n.message.contains("nonexistent.rs")
        }));
    }

    #[test]
    fn empty_diff_on_empty_store_is_not_degraded() {
        // No changed files → nothing to assess → the empty-index guard must not
        // fire (an empty diff is a legitimate clean no-op, not a degraded run).
        let store = GraphStore::in_memory().expect("in_memory store");
        let result = analyze_blast_radius(&store, &[], &opts(None, 3, false), None, None).unwrap();
        assert_eq!(result.status, AnalysisStatus::Complete);
        assert_eq!(result.gate_state, GateState::Ok);
    }

    #[test]
    fn analyze_blast_radius_with_symbols() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        let sym_a = Symbol {
            uid: "sym:a".to_string(),
            name: "fn_a".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/a.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn fn_a()".to_string(),
            summary: None,
            content_hash: "h1".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        let sym_b = Symbol {
            uid: "sym:b".to_string(),
            name: "fn_b".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/b.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn fn_b()".to_string(),
            summary: None,
            content_hash: "h2".to_string(),
            embedding: None,
            pagerank_score: Some(0.1),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        store.insert_symbol(&sym_a).expect("insert sym_a");
        store.insert_symbol(&sym_b).expect("insert sym_b");

        // sym_b calls sym_a, so changing a.rs should show sym_b as affected
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:b".to_string(),
                target_uid: "sym:a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .expect("insert edge");

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result.changed_symbols.len(), 1);
        assert_eq!(result.changed_symbols[0].name, "fn_a");
        assert_eq!(result.affected_symbols.len(), 1);
        assert_eq!(result.affected_symbols[0].name, "fn_b");
        // With a live ranking cache, centrality is judged RELATIVE to the
        // graph mean (nw-059). This 2-node in-memory graph computes uniform
        // ranks, so the changed symbol is exactly average — no boost, and the
        // hand-set row score (0.5) is superseded by the cache hydration.
        assert_eq!(result.risk_level, RiskLevel::Low);

        // Verify impact_score is populated: sym_b calls sym_a with confidence 0.9,
        // so impact_score should be 1.0 * 0.9 = 0.9.
        let score = result.affected_symbols[0].impact_score;
        assert!(
            (score - 0.9).abs() < 1e-6,
            "expected impact_score ~0.9, got {score}"
        );
    }

    #[test]
    fn org_wide_populated_for_cross_repo_impact() {
        use nestweaver_schema::{
            CrossRepoLinkType, EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility,
        };

        let store = GraphStore::in_memory().expect("in_memory store");

        let mk = |uid: &str, name: &str, repo: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo.to_string(),
            file_path: file.to_string(),
            start_line: 3,
            end_line: 9,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: Some(0.2),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        // A symbol in repo:api is consumed by a symbol in repo:client via a
        // cross-repo link. Changing the api symbol must surface repo:client as
        // an org-wide (cross-repo) impact.
        store
            .insert_symbol(&mk("api", "Handler", "repo:api", "src/api.rs"))
            .unwrap();
        store
            .insert_symbol(&mk("client", "Caller", "repo:client", "src/client.rs"))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "client".to_string(),
                target_uid: "api".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/api.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();

        let org = result
            .org_wide
            .expect("org_wide must be populated when a change impacts another repo");
        assert!(
            org.impacted_repos.iter().any(|r| r == "repo:client"),
            "impacted_repos should include the downstream repo; got: {:?}",
            org.impacted_repos
        );
        let all_items = org.breaking.len() + org.warnings.len() + org.info.len();
        assert!(all_items >= 1, "expected at least one org-wide impact item");
        assert!(
            org.breaking
                .iter()
                .chain(&org.warnings)
                .chain(&org.info)
                .any(|i| i.affected_name == "Caller" && i.affected_repo == "repo:client"),
            "org-wide item should describe the cross-repo consumer"
        );
        let item = org
            .breaking
            .iter()
            .chain(&org.warnings)
            .chain(&org.info)
            .find(|i| i.affected_name == "Caller")
            .expect("cross-repo item");
        assert_eq!(item.change_repo_uid, "repo:api");
        assert_eq!(item.affected_repo_uid, "repo:client");
    }

    #[test]
    fn batched_lookup_preserves_kinds_and_repos() {
        use nestweaver_schema::{
            CrossRepoLinkType, EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility,
        };

        let store = GraphStore::in_memory().expect("in_memory store");

        let mk = |uid: &str, name: &str, kind: SymbolKind, repo: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind,
            repo_uid: repo.to_string(),
            file_path: file.to_string(),
            start_line: 3,
            end_line: 9,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: Some(0.2),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        // Changing `api` (repo:api) affects two downstream symbols with distinct
        // kinds across two repos: a Method in the SAME repo (via Calls) and a
        // Class in ANOTHER repo (via a cross-repo link). The batch lookup must
        // populate each affected symbol's kind + repo_uid identically to the old
        // per-node path, and the cross-repo one must still trip org_wide.
        store
            .insert_symbol(&mk(
                "api",
                "Handler",
                SymbolKind::Function,
                "repo:api",
                "src/api.rs",
            ))
            .unwrap();
        store
            .insert_symbol(&mk(
                "helper",
                "Helper",
                SymbolKind::Method,
                "repo:api",
                "src/helper.rs",
            ))
            .unwrap();
        store
            .insert_symbol(&mk(
                "client",
                "Caller",
                SymbolKind::Class,
                "repo:client",
                "src/client.rs",
            ))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "helper".to_string(),
                target_uid: "api".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "client".to_string(),
                target_uid: "api".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/api.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();

        let helper = result
            .affected_symbols
            .iter()
            .find(|s| s.uid == "helper")
            .expect("helper must be in the affected set");
        assert_eq!(helper.kind, "Method", "batch lookup must populate kind");
        assert_eq!(
            helper.repo_uid, "repo:api",
            "batch lookup must populate repo_uid"
        );

        let client = result
            .affected_symbols
            .iter()
            .find(|s| s.uid == "client")
            .expect("client must be in the affected set");
        assert_eq!(client.kind, "Class", "batch lookup must populate kind");
        assert_eq!(
            client.repo_uid, "repo:client",
            "batch lookup must populate repo_uid"
        );

        // Cross-repo org-wide detection still fires off the batch-populated repos.
        let org = result
            .org_wide
            .expect("org_wide must be populated for the cross-repo consumer");
        assert!(
            org.impacted_repos.iter().any(|r| r == "repo:client"),
            "impacted_repos should include the downstream repo; got: {:?}",
            org.impacted_repos
        );
        assert!(
            org.breaking
                .iter()
                .chain(&org.warnings)
                .chain(&org.info)
                .any(|i| i.affected_name == "Caller" && i.affected_repo == "repo:client"),
            "org-wide item should describe the cross-repo consumer"
        );
    }

    #[test]
    fn org_wide_none_when_impact_stays_in_repo() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&mk("a", "fn_a", "src/a.rs")).unwrap();
        store.insert_symbol(&mk("b", "fn_b", "src/b.rs")).unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "b".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();
        assert!(
            result.org_wide.is_none(),
            "org_wide must stay None when all impact is within one repo"
        );
    }

    #[test]
    fn impact_score_decays_through_chain() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        // Build chain: C --0.8--> B --0.9--> A
        // Changing A should affect B (score 0.9) and C (score 0.9 * 0.8 = 0.72).
        let make_sym = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        for (uid, name, file) in [
            ("sym:a", "fn_a", "src/a.rs"),
            ("sym:b", "fn_b", "src/b.rs"),
            ("sym:c", "fn_c", "src/c.rs"),
        ] {
            store.insert_symbol(&make_sym(uid, name, file)).unwrap();
        }

        // B calls A (confidence 0.9)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:b".to_string(),
                target_uid: "sym:a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // C calls B (confidence 0.8)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:c".to_string(),
                target_uid: "sym:b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.8,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 5, false),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result.affected_symbols.len(), 2);
        // Results should be sorted by impact_score descending.
        assert_eq!(result.affected_symbols[0].name, "fn_b");
        assert!((result.affected_symbols[0].impact_score - 0.9).abs() < 1e-6);
        assert_eq!(result.affected_symbols[1].name, "fn_c");
        assert!((result.affected_symbols[1].impact_score - 0.72).abs() < 1e-6);
    }

    #[test]
    fn low_confidence_chain_pruned_below_threshold() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");

        // Build chain: C --0.2--> B --0.3--> A
        // B's score = 0.3, C's candidate score = 0.3 * 0.2 = 0.06 < 0.10 threshold.
        // So C should be pruned.
        let make_sym = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        for (uid, name, file) in [
            ("sym:a", "fn_a", "src/a.rs"),
            ("sym:b", "fn_b", "src/b.rs"),
            ("sym:c", "fn_c", "src/c.rs"),
        ] {
            store.insert_symbol(&make_sym(uid, name, file)).unwrap();
        }

        // B calls A (confidence 0.3)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:b".to_string(),
                target_uid: "sym:a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.3,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // C calls B (confidence 0.2)
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "sym:c".to_string(),
                target_uid: "sym:b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.2,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 5, false),
            None,
            None,
        )
        .unwrap();

        // B is included (score 0.3 >= 0.10), but C is pruned (score 0.06 < 0.10).
        assert_eq!(result.affected_symbols.len(), 1);
        assert_eq!(result.affected_symbols[0].name, "fn_b");
        assert!((result.affected_symbols[0].impact_score - 0.3).abs() < 1e-6);
    }

    #[test]
    fn target_repo_scopes_changed_file_resolution() {
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};

        // Same relative path `src/main.rs` lives in two repos, each owning a
        // distinct symbol — the unified multi-repo graph scenario.
        let store = GraphStore::in_memory().expect("in_memory store");

        let sym_r1 = Symbol {
            uid: "sym:r1_main".to_string(),
            name: "main_r1".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: "src/main.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn main()".to_string(),
            summary: None,
            content_hash: "h1".to_string(),
            embedding: None,
            pagerank_score: Some(0.1),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        let sym_r2 = Symbol {
            uid: "sym:r2_main".to_string(),
            name: "main_r2".to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:2".to_string(),
            file_path: "src/main.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn main()".to_string(),
            summary: None,
            content_hash: "h2".to_string(),
            embedding: None,
            pagerank_score: Some(0.1),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        store.insert_symbol(&sym_r1).expect("insert sym_r1");
        store.insert_symbol(&sym_r2).expect("insert sym_r2");

        // Scoped to repo:1 — only repo:1's symbol resolves.
        let scoped = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/main.rs")],
            &opts(Some("repo:1"), 3, false),
            None,
            None,
        )
        .unwrap();
        let scoped_names: HashSet<&str> = scoped
            .changed_symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(scoped.changed_symbols.len(), 1);
        assert!(scoped_names.contains("main_r1"));
        assert!(!scoped_names.contains("main_r2"));

        // Unscoped (None) — the historical behavior picks up both repos.
        let unscoped = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/main.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();
        let unscoped_names: HashSet<&str> = unscoped
            .changed_symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(unscoped.changed_symbols.len(), 2);
        assert!(unscoped_names.contains("main_r1"));
        assert!(unscoped_names.contains("main_r2"));
    }

    #[test]
    fn status_complete_and_gate_ok_on_clean_analysis() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&mk("a", "fn_a", "src/a.rs")).unwrap();
        store.insert_symbol(&mk("b", "fn_b", "src/b.rs")).unwrap();
        // b calls a — changing a.rs affects b, a healthy resolvable run.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "b".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result.status, AnalysisStatus::Complete);
        assert!(
            result.notifications.is_empty(),
            "clean run must have no notifications, got: {:?}",
            result.notifications
        );
        // Risk is Low (1 affected, no pagerank), so the gate is Ok.
        assert!(matches!(
            result.gate_state,
            GateState::Ok | GateState::RiskFlagged
        ));
        assert_eq!(result.gate_state, GateState::Ok);
    }

    #[test]
    fn zero_symbols_in_indexed_repo_is_partial() {
        use nestweaver_schema::{Repo, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        // An indexed repo (visible via list_repos) that owns one symbol.
        store
            .insert_repo(&Repo {
                uid: "repo:1".to_string(),
                url: "https://example.com/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                staleness_commits_behind: 0,
                instance_id: "inst-1".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: "sym:a".to_string(),
                name: "fn_a".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:1".to_string(),
                file_path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn fn_a()".to_string(),
                summary: None,
                content_hash: "h_a".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();

        // Analyze a DIFFERENT changed path, scoped to the indexed repo: the
        // lookup succeeds but resolves 0 symbols — path drift / unindexed file.
        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/other.rs")],
            &opts(Some("repo:1"), 3, false),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result.status, AnalysisStatus::Partial);
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "changed-file-no-symbols"),
            "expected a changed-file-no-symbols notification, got: {:?}",
            result.notifications
        );
        // A non-Complete run is never Ok/RiskFlagged — it is DegradedUnknown.
        assert_eq!(result.gate_state, GateState::DegradedUnknown);
    }

    #[test]
    fn zero_symbols_non_source_file_stays_complete() {
        use nestweaver_schema::Repo;

        let store = GraphStore::in_memory().expect("in_memory store");
        store
            .insert_repo(&Repo {
                uid: "repo:1".to_string(),
                url: "https://example.com/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                staleness_commits_behind: 0,
                instance_id: "inst-1".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();

        // A docs/config file resolving to 0 symbols is expected, not drift, so
        // it must NOT degrade the gate — otherwise most healthy PRs (which touch
        // markdown/config/lockfiles) would gate as DegradedUnknown.
        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("README.md")],
            &opts(Some("repo:1"), 3, false),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result.status, AnalysisStatus::Complete);
        assert!(
            !result
                .notifications
                .iter()
                .any(|n| n.descriptor == "changed-file-no-symbols"),
            "a non-source file must not emit a drift notification: {:?}",
            result.notifications
        );
        assert_eq!(result.gate_state, GateState::Ok);
    }

    #[test]
    fn unscoped_changed_source_without_symbols_is_unknown() {
        use nestweaver_schema::Repo;

        let store = GraphStore::in_memory().expect("store");
        store
            .insert_repo(&Repo {
                uid: "repo:1".to_string(),
                url: "https://example.com/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                staleness_commits_behind: 0,
                instance_id: "inst-1".to_string(),
                name: None,
                root_path: None,
            })
            .expect("insert repo");

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/new_module.rs")],
            &BlastRadiusOptions::default(),
            None,
            None,
        )
        .expect("analysis");

        assert_eq!(result.status, AnalysisStatus::Partial);
        assert_eq!(result.gate_state, GateState::DegradedUnknown);
        assert!(result.notifications.iter().any(|n| {
            n.level == NotificationLevel::Warning
                && n.descriptor == "changed-file-no-symbols"
                && n.message.contains("src/new_module.rs")
        }));
    }

    #[test]
    fn coverage_reports_repos_in_scope_and_direction() {
        use nestweaver_schema::{EdgeType, Repo, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        // The owning repo is indexed and up to date, so NotIndexed must not fire.
        store
            .insert_repo(&Repo {
                uid: "repo:1".to_string(),
                url: "https://example.com/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                staleness_commits_behind: 0,
                instance_id: "inst-1".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&mk("a", "fn_a", "src/a.rs")).unwrap();
        store.insert_symbol(&mk("b", "fn_b", "src/b.rs")).unwrap();
        // b calls a — a healthy, fully-resolvable run.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "b".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result.analysis_direction, "over-approximate");
        assert!(
            result
                .coverage
                .repos_in_scope
                .contains(&"repo:1".to_string()),
            "in-scope repos should include repo:1, got: {:?}",
            result.coverage.repos_in_scope
        );
        assert!(
            !result.coverage.traversal_truncated,
            "a complete walk must not report truncation"
        );
        for bs in [
            BlindSpot::DynamicDispatch,
            BlindSpot::Reflection,
            BlindSpot::ConfigWiring,
            BlindSpot::Codegen,
        ] {
            assert!(
                result.blind_spots.contains(&bs),
                "static-analysis blind spot {bs:?} must always be present"
            );
        }
        assert!(
            !result
                .blind_spots
                .contains(&BlindSpot::PrunedBelowThreshold),
            "a complete walk must not flag PrunedBelowThreshold"
        );
        assert!(
            !result.blind_spots.contains(&BlindSpot::DepthTruncated),
            "a complete walk must not flag DepthTruncated"
        );
        assert!(
            !result.blind_spots.contains(&BlindSpot::NotIndexed),
            "a fully-indexed run must not flag NotIndexed"
        );
    }

    #[test]
    fn truncated_traversal_sets_coverage_flag_and_blind_spot() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        // Chain c → b → a → target, all high-confidence, deeper than max_depth=2
        // so a frontier node is left unexpanded at the boundary.
        for (uid, file) in [
            ("target", "src/target.rs"),
            ("a", "src/a.rs"),
            ("b", "src/b.rs"),
            ("c", "src/c.rs"),
        ] {
            store.insert_symbol(&mk(uid, uid, file)).unwrap();
        }
        for (src, tgt) in [("a", "target"), ("b", "a"), ("c", "b")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: tgt.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/target.rs")],
            &opts(None, 2, false),
            None,
            None,
        )
        .unwrap();

        assert!(
            result.coverage.traversal_truncated,
            "a chain deeper than max_depth must set traversal_truncated"
        );
        assert!(
            result.blind_spots.contains(&BlindSpot::DepthTruncated),
            "a depth-capped traversal must flag DepthTruncated, got: {:?}",
            result.blind_spots
        );

        // nw-105: the same run must not simultaneously claim it completed.
        // Before the fix, coverage.traversal_truncated and the DepthTruncated
        // blind spot were both set while status stayed Complete, so
        // derive_gate_state returned Ok — a merge gate read "safe" for an
        // analysis that never finished.
        assert_ne!(
            result.status,
            AnalysisStatus::Complete,
            "a truncated traversal must not report status Complete — it did not complete"
        );
        assert_eq!(
            result.gate_state,
            GateState::DegradedUnknown,
            "a truncated traversal must gate as degraded-unknown, never ok; \
             got status {:?} / gate {:?}",
            result.status,
            result.gate_state
        );
        assert!(
            !result
                .blind_spots
                .contains(&BlindSpot::PrunedBelowThreshold),
            "a depth-only truncation must NOT be mislabeled PrunedBelowThreshold, got: {:?}",
            result.blind_spots
        );
    }

    #[test]
    fn stale_repo_appears_in_coverage() {
        use nestweaver_schema::{Repo, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        // An indexed repo whose graph is 7 commits behind source.
        store
            .insert_repo(&Repo {
                uid: "repo:stale".to_string(),
                url: "https://example.com/stale".to_string(),
                indexed_sha: "old123".to_string(),
                staleness_commits_behind: 7,
                instance_id: "inst-1".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: "sym:a".to_string(),
                name: "fn_a".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:stale".to_string(),
                file_path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn fn_a()".to_string(),
                summary: None,
                content_hash: "h_a".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();

        let stale = result
            .coverage
            .stale_repos
            .iter()
            .find(|s| s.repo_uid == "repo:stale")
            .expect("the stale repo owning a changed symbol must appear in stale_repos");
        assert_eq!(stale.commits_behind, 7);
    }

    #[test]
    fn unindexed_target_repo_flagged() {
        use nestweaver_schema::{Repo, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        // A known, indexed repo exists, but the analysis targets a different,
        // absent repo.
        store
            .insert_repo(&Repo {
                uid: "repo:known".to_string(),
                url: "https://example.com/known".to_string(),
                indexed_sha: "abc123".to_string(),
                staleness_commits_behind: 0,
                instance_id: "inst-1".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: "sym:a".to_string(),
                name: "fn_a".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:known".to_string(),
                file_path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn fn_a()".to_string(),
                summary: None,
                content_hash: "h_a".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(Some("repo:absent"), 3, false),
            None,
            None,
        )
        .unwrap();

        assert!(
            result
                .coverage
                .repos_not_indexed
                .contains(&"repo:absent".to_string()),
            "the unindexed target repo must appear in repos_not_indexed, got: {:?}",
            result.coverage.repos_not_indexed
        );
        assert!(
            result.blind_spots.contains(&BlindSpot::NotIndexed),
            "an unindexed referenced repo must flag NotIndexed, got: {:?}",
            result.blind_spots
        );
    }

    #[test]
    fn degraded_run_never_risk_flagged() {
        // The NON-NEGOTIABLE rule: a run that did not complete is never
        // RiskFlagged, regardless of the computed risk — it is DegradedUnknown.
        assert_eq!(
            derive_gate_state(AnalysisStatus::Degraded, RiskLevel::High),
            GateState::DegradedUnknown
        );
        assert_eq!(
            derive_gate_state(AnalysisStatus::Failed, RiskLevel::High),
            GateState::DegradedUnknown
        );
        assert_eq!(
            derive_gate_state(AnalysisStatus::Partial, RiskLevel::High),
            GateState::DegradedUnknown
        );
        // Complete runs still map risk faithfully.
        assert_eq!(
            derive_gate_state(AnalysisStatus::Complete, RiskLevel::High),
            GateState::RiskFlagged
        );
        assert_eq!(
            derive_gate_state(AnalysisStatus::Complete, RiskLevel::Low),
            GateState::Ok
        );
    }

    #[test]
    fn data_edges_surface_type_reference_dependent() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&mk("a", "fn_a", "src/a.rs")).unwrap();
        store.insert_symbol(&mk("b", "fn_b", "src/b.rs")).unwrap();
        // b only references a's type (Uses) — there is no call edge, so the
        // structural-only walk cannot reach it.
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "b".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Uses,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // Default off: the type-reference dependent is a false negative.
        let without = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();
        assert!(
            !without.affected_symbols.iter().any(|s| s.name == "fn_b"),
            "with include_data_edges=false, the Uses-only dependent must be absent"
        );

        // Data tier on: the type-reference dependent surfaces (false-negative fix).
        let with = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, true),
            None,
            None,
        )
        .unwrap();
        assert!(
            with.affected_symbols.iter().any(|s| s.name == "fn_b"),
            "with include_data_edges=true, the Uses-only dependent must surface; got: {:?}",
            with.affected_symbols
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    fn insert_minimal_symbol(store: &GraphStore, uid: &str, name: &str, file: &str) {
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};
        store
            .insert_symbol(&Symbol {
                uid: uid.into(),
                name: name.into(),
                kind: SymbolKind::Function,
                repo_uid: "repo:1".into(),
                file_path: file.into(),
                start_line: 1,
                end_line: 1,
                signature: format!("fn {name}()"),
                summary: None,
                content_hash: format!("h_{uid}"),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .expect("insert symbol");
    }

    #[test]
    fn cochange_sidecar_surfaces_coupled_files() {
        use crate::cochange::{CoChangeEdge, save_cochange_sidecar};
        let store = GraphStore::in_memory().expect("store");
        insert_minimal_symbol(&store, "sym:bill", "compute_total", "src/billing.rs");
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("scratch.lbug");
        std::fs::write(&db, b"").expect("touch db");
        let edges = vec![CoChangeEdge {
            repo: String::new(),
            file_a: "src/billing.rs".into(),
            file_b: "src/invoice_templates.sql".into(),
            cochange_count: 9,
            total_commits_a: 12,
            total_commits_b: 10,
            confidence: 0.69,
        }];
        save_cochange_sidecar(&edges, &crate::sidecar_path(&db, ".cochange.json"))
            .expect("save sidecar");

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/billing.rs")],
            &BlastRadiusOptions::default(),
            None,
            Some(&db),
        )
        .expect("analyze");

        assert_eq!(result.cochanged_files.len(), 1);
        let c = &result.cochanged_files[0];
        assert_eq!(c.file, "src/invoice_templates.sql");
        assert_eq!(c.coupled_to, "src/billing.rs");
        assert_eq!(c.cochange_count, 9);
        assert!((c.confidence - 0.69).abs() < 1e-6);
        assert!(c.note.contains("no static edge"));
    }

    /// nw-062: the sidecar exists but holds another repo's history. This is the
    /// 33-of-34 case on the real multi-repo database — the run took the "loaded"
    /// branch, matched nothing, and disclosed nothing, so a caller could not
    /// tell "no coupling" from "no data".
    #[test]
    fn a_sidecar_that_does_not_cover_the_changed_file_is_disclosed() {
        use crate::cochange::{CoChangeEdge, save_cochange_sidecar};
        let store = GraphStore::in_memory().expect("store");
        insert_minimal_symbol(
            &store,
            "sym:view",
            "WorkflowsView",
            "src/app/WorkflowsView.tsx",
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("scratch.lbug");
        std::fs::write(&db, b"").expect("touch db");

        // History mined from a DIFFERENT repo than the file being analysed.
        let edges = vec![CoChangeEdge {
            repo: String::new(),
            file_a: "crates/nestweaver-store/build.rs".into(),
            file_b: "crates/nestweaver-store/Cargo.toml".into(),
            cochange_count: 6,
            total_commits_a: 10,
            total_commits_b: 10,
            confidence: 0.60,
        }];
        save_cochange_sidecar(&edges, &crate::sidecar_path(&db, ".cochange.json"))
            .expect("save sidecar");

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/app/WorkflowsView.tsx")],
            &BlastRadiusOptions::default(),
            None,
            Some(&db),
        )
        .expect("analyze");

        assert!(result.cochanged_files.is_empty());
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "cochange-no-coverage"),
            "a sidecar with no history for this file must be disclosed: {:?}",
            result.notifications
        );
        // Advisory tier: absence never degrades the run.
        assert_eq!(result.status, AnalysisStatus::Complete);
    }

    /// The complement: when the sidecar DOES cover the changed file, an empty or
    /// populated result is trustworthy and must not carry the caveat — a note
    /// that always fires teaches callers to ignore it.
    #[test]
    fn a_covered_file_does_not_carry_the_no_coverage_caveat() {
        use crate::cochange::{CoChangeEdge, save_cochange_sidecar};
        let store = GraphStore::in_memory().expect("store");
        insert_minimal_symbol(&store, "sym:bill", "compute_total", "src/billing.rs");
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("scratch.lbug");
        std::fs::write(&db, b"").expect("touch db");
        let edges = vec![CoChangeEdge {
            repo: String::new(),
            file_a: "src/billing.rs".into(),
            file_b: "src/invoice_templates.sql".into(),
            cochange_count: 9,
            total_commits_a: 12,
            total_commits_b: 10,
            confidence: 0.69,
        }];
        save_cochange_sidecar(&edges, &crate::sidecar_path(&db, ".cochange.json"))
            .expect("save sidecar");

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/billing.rs")],
            &BlastRadiusOptions::default(),
            None,
            Some(&db),
        )
        .expect("analyze");

        assert_eq!(result.cochanged_files.len(), 1);
        assert!(
            !result
                .notifications
                .iter()
                .any(|n| n.descriptor == "cochange-no-coverage"),
            "a covered file must not be told its history is missing: {:?}",
            result.notifications
        );
    }

    #[test]
    fn missing_cochange_sidecar_emits_honesty_note_not_error() {
        let store = GraphStore::in_memory().expect("store");
        insert_minimal_symbol(&store, "sym:bill", "compute_total", "src/billing.rs");
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("scratch.lbug");
        std::fs::write(&db, b"").expect("touch db");
        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/billing.rs")],
            &BlastRadiusOptions::default(),
            None,
            Some(&db),
        )
        .expect("analyze");
        assert!(result.cochanged_files.is_empty());
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "cochange-unavailable"),
            "absent sidecar must be disclosed, not silent: {:?}",
            result.notifications
        );
        // Advisory tier: absence never degrades the run.
        assert_eq!(result.status, AnalysisStatus::Complete);
    }

    #[test]
    fn limit_does_not_deflate_risk_or_gate() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};
        let store = GraphStore::in_memory().expect("store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.into(),
            name: name.into(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".into(),
            file_path: file.into(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store
            .insert_symbol(&mk("target", "fn_target", "src/target.rs"))
            .unwrap();
        for i in 0..60 {
            let uid = format!("c{i}");
            store
                .insert_symbol(&mk(&uid, &format!("fn_c{i}"), &format!("src/c{i}.rs")))
                .unwrap();
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: uid,
                    target_uid: "target".into(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }
        let files = [PathBuf::from("src/target.rs")];
        let no_limit = analyze_blast_radius(
            &store,
            &files,
            &BlastRadiusOptions {
                limit: None,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        let with_limit = analyze_blast_radius(
            &store,
            &files,
            &BlastRadiusOptions {
                limit: Some(5),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(no_limit.risk_level, RiskLevel::High);
        assert_eq!(no_limit.gate_state, GateState::RiskFlagged);
        // A display cap must never change the verdict (safe-RTS: verdicts from
        // truncated sets are non-safe by definition).
        assert_eq!(with_limit.risk_level, no_limit.risk_level);
        assert_eq!(with_limit.gate_state, no_limit.gate_state);
        assert_eq!(with_limit.summary, no_limit.summary);
        assert_eq!(with_limit.affected_symbol_count, 60);
        assert_eq!(
            with_limit.affected_symbols.len(),
            5,
            "display cap still applies"
        );
        // The human summary must report the TRUE total, not the capped count.
        assert!(
            with_limit.summary.contains("60 transitively affected"),
            "summary must carry the pre-cap total, got: {}",
            with_limit.summary
        );
    }

    #[test]
    fn changed_symbol_pagerank_hydrates_from_cache() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};
        let store = GraphStore::in_memory().expect("store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.into(),
            name: name.into(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".into(),
            file_path: file.into(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            // Rows carry no score — the production shape (scores live only in
            // the ranking cache/sidecar).
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store
            .insert_symbol(&mk("hub", "hub_fn", "src/hub.rs"))
            .unwrap();
        for i in 0..5 {
            let uid = format!("caller{i}");
            store
                .insert_symbol(&mk(
                    &uid,
                    &format!("caller_fn{i}"),
                    &format!("src/caller{i}.rs"),
                ))
                .unwrap();
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: uid,
                    target_uid: "hub".into(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }
        store.ensure_pagerank_loaded();
        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/hub.rs")],
            &BlastRadiusOptions::default(),
            None,
            None,
        )
        .unwrap();
        let hub = result
            .changed_symbols
            .iter()
            .find(|s| s.uid == "hub")
            .expect("hub in changed set");
        assert!(
            hub.pagerank_score.unwrap_or(0.0) > 0.0,
            "changed-symbol pagerank must hydrate from the cache (row value is None): {:?}",
            hub.pagerank_score
        );
    }

    #[test]
    fn limit_caps_affected_symbols_with_notification() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        // Change `target`; three distinct callers depend on it directly with
        // decreasing confidence so their impact_scores order deterministically.
        store
            .insert_symbol(&mk("target", "fn_target", "src/target.rs"))
            .unwrap();
        for (uid, name, file, conf) in [
            ("d1", "fn_d1", "src/d1.rs", 0.9_f32),
            ("d2", "fn_d2", "src/d2.rs", 0.8),
            ("d3", "fn_d3", "src/d3.rs", 0.7),
        ] {
            store.insert_symbol(&mk(uid, name, file)).unwrap();
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: uid.to_string(),
                    target_uid: "target".to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: conf,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }

        let options = BlastRadiusOptions {
            target_repo: None,
            max_depth: 3,
            include_data_edges: false,
            limit: Some(2),
        };
        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/target.rs")],
            &options,
            None,
            None,
        )
        .unwrap();

        // Only the top-2 by impact_score survive the cap.
        assert_eq!(result.affected_symbols.len(), 2);
        assert_eq!(result.affected_symbols[0].name, "fn_d1");
        assert_eq!(result.affected_symbols[1].name, "fn_d2");
        // The cap emits a note carrying the true total…
        let note = result
            .notifications
            .iter()
            .find(|n| n.descriptor == "results-truncated")
            .expect("a results-truncated notification must be emitted");
        assert!(
            note.message.contains("of 3"),
            "the truncation note must mention the total, got: {}",
            note.message
        );
        // …but a display cap is not a failure, so the run stays Complete.
        assert_eq!(result.status, AnalysisStatus::Complete);
    }

    #[test]
    fn cancelled_traversal_marks_degraded() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&mk("a", "fn_a", "src/a.rs")).unwrap();
        store.insert_symbol(&mk("b", "fn_b", "src/b.rs")).unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "b".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();

        // Pre-tripped deadline: the traversal is cancelled before it completes.
        let cancel = Arc::new(AtomicBool::new(true));
        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            Some(&cancel),
            None,
        )
        .unwrap();

        assert_eq!(result.status, AnalysisStatus::Degraded);
        assert!(
            result
                .notifications
                .iter()
                .any(|n| n.descriptor == "analysis-cancelled"),
            "a cancelled run must emit an analysis-cancelled notification, got: {:?}",
            result.notifications
        );
        // A degraded run is never RiskFlagged — it is DegradedUnknown.
        assert_eq!(result.gate_state, GateState::DegradedUnknown);
    }

    // ── feature-level depth truncation (end-to-end) ─────────────────────

    /// End-to-end through `analyze_blast_radius`: a call chain deeper than
    /// `max_depth` must (a) include only the dependents within `max_depth` in
    /// `affected_symbols`, and (b) surface the incompleteness — `coverage`
    /// flags the truncation and `blind_spots` names `DepthTruncated`.
    /// (Distinct from the store-level `impact_detailed_flags_depth_truncation`:
    /// this asserts the affected-set membership the feature actually returns.)
    #[test]
    fn depth_truncation_limits_affected_set_end_to_end() {
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        // Chain: far → mid → near → target (near depth 1, mid depth 2, far
        // depth 3). With max_depth=2 the walk reaches near+mid but leaves the
        // frontier at mid unexpanded, so far never appears.
        for (uid, name, file) in [
            ("target", "fn_target", "src/target.rs"),
            ("near", "fn_near", "src/near.rs"),
            ("mid", "fn_mid", "src/mid.rs"),
            ("far", "fn_far", "src/far.rs"),
        ] {
            store.insert_symbol(&mk(uid, name, file)).unwrap();
        }
        for (src, tgt) in [("near", "target"), ("mid", "near"), ("far", "mid")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: src.to_string(),
                    target_uid: tgt.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/target.rs")],
            &opts(None, 2, false),
            None,
            None,
        )
        .unwrap();

        let affected: HashSet<&str> = result
            .affected_symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            affected.contains("fn_near") && affected.contains("fn_mid"),
            "dependents within max_depth must appear; got: {affected:?}"
        );
        assert!(
            !affected.contains("fn_far"),
            "a dependent beyond max_depth must be truncated out; got: {affected:?}"
        );
        assert!(
            result.coverage.traversal_truncated,
            "a chain deeper than max_depth must set coverage.traversal_truncated"
        );
        assert!(
            result.blind_spots.contains(&BlindSpot::DepthTruncated),
            "a depth-truncated feature run must flag DepthTruncated, got: {:?}",
            result.blind_spots
        );
    }

    // ── classify_org_severity boundaries ────────────────────────────────

    /// Exercise `classify_org_severity` directly at its cutoffs. The function
    /// is `>= 0.5 → breaking`, `>= 0.25 → warning`, else `info`.
    #[test]
    fn classify_org_severity_boundaries() {
        // Breaking cutoff at 0.5 (inclusive).
        assert_eq!(classify_org_severity(0.5), "breaking");
        assert_eq!(classify_org_severity(0.49), "warning");
        // Warning cutoff at 0.25 (inclusive).
        assert_eq!(classify_org_severity(0.25), "warning");
        assert_eq!(classify_org_severity(0.24), "info");
        // Clearly-high and clearly-low ends of the range.
        assert_eq!(classify_org_severity(0.99), "breaking");
        assert_eq!(classify_org_severity(0.0), "info");
    }

    // ── changed_files_from_git ──────────────────────────────────────────

    /// Whether `git` is on PATH; tests that shell out to git skip gracefully
    /// when it is not (e.g. a minimal CI image).
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run a git subcommand in `dir`, asserting it succeeds.
    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `changed_files_from_git` over a real temp repo: the working-tree case
    /// (`None`), the base-ref case (`Some(base)`), the empty-diff case, and the
    /// non-repo error case.
    #[test]
    fn changed_files_from_git_working_tree_and_base_ref() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();

        // Init an isolated repo (no signing / template surprises) with a
        // committed baseline file.
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(repo, &["add", "a.txt"]);
        git(repo, &["commit", "-q", "-m", "first"]);

        // Working-tree case (None): an unstaged modification to a tracked file
        // shows up in `git diff --name-only`.
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        let working = changed_files_from_git(repo, None).unwrap();
        assert_eq!(
            working,
            vec![PathBuf::from("a.txt")],
            "working-tree diff must list the modified tracked file"
        );

        // Commit a second change (modify a.txt, add b.txt) so a base-ref diff
        // has two files between HEAD~1 and the working tree.
        git(repo, &["add", "a.txt"]);
        std::fs::write(repo.join("b.txt"), "bee\n").unwrap();
        git(repo, &["add", "b.txt"]);
        git(repo, &["commit", "-q", "-m", "second"]);

        // Base-ref case (Some): diff against the first commit lists both files.
        let mut against_base = changed_files_from_git(repo, Some("HEAD~1")).unwrap();
        against_base.sort();
        assert_eq!(
            against_base,
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
            "base-ref diff must list every file changed since the base"
        );

        // Empty-diff case: a clean working tree against HEAD yields no files.
        let empty = changed_files_from_git(repo, Some("HEAD")).unwrap();
        assert!(
            empty.is_empty(),
            "a clean tree against HEAD must yield an empty vec, got: {empty:?}"
        );

        // Error case: a path that is not a git repo must return Err, never a
        // silent empty vec.
        let non_repo = tempfile::tempdir().expect("tempdir");
        let err = changed_files_from_git(non_repo.path(), None);
        assert!(
            err.is_err(),
            "git diff outside a repository must return Err, got: {err:?}"
        );
    }

    // ── cluster wiring end-to-end ───────────────────────────────────────

    /// Exercise the `load_clusters` + `compute_affected_clusters` path through
    /// `analyze_blast_radius` (the other tests pass `db_path = None`, so it is
    /// never hit). Cluster data is written to a real sidecar next to a temp db
    /// path; the changed + affected symbols land in >3 clusters, so
    /// `affected_clusters` is populated AND the cluster risk boost fires
    /// (base Low → Medium).
    #[test]
    fn affected_clusters_populated_from_sidecar_boosts_risk() {
        use crate::cluster_dispatch::{
            ClusterMember, ClusteringOutput, CommunityInfo, save_clusters,
        };
        use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:1".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        // Change `target`; four callers depend on it directly. Each of the five
        // symbols lives in its own cluster, so five clusters are touched (>3).
        store
            .insert_symbol(&mk("target", "fn_target", "src/target.rs"))
            .unwrap();
        for (uid, file) in [
            ("d1", "src/d1.rs"),
            ("d2", "src/d2.rs"),
            ("d3", "src/d3.rs"),
            ("d4", "src/d4.rs"),
        ] {
            store.insert_symbol(&mk(uid, uid, file)).unwrap();
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: uid.to_string(),
                    target_uid: "target".to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }

        // Write cluster data to the sidecar for a temp db path: one cluster per
        // symbol so all five are "touched" (the changed target + 4 affected).
        let member = |uid: &str, file: &str| ClusterMember {
            uid: uid.to_string(),
            name: uid.to_string(),
            file_path: file.to_string(),
            kind: "Function".to_string(),
        };
        let communities: Vec<CommunityInfo> = [
            ("target", "src/target.rs"),
            ("d1", "src/d1.rs"),
            ("d2", "src/d2.rs"),
            ("d3", "src/d3.rs"),
            ("d4", "src/d4.rs"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (uid, file))| CommunityInfo {
            id: i as u32,
            name: format!("cluster-{i}"),
            cohesion: 0.8,
            member_count: 1,
            members: vec![member(uid, file)],
            key_files: vec![file.to_string()],
        })
        .collect();
        let clustering = ClusteringOutput {
            resolution: 1.0,
            modularity: 0.5,
            communities,
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("graph.lbug");
        save_clusters(&db_path, &clustering).expect("write clusters sidecar");

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/target.rs")],
            &opts(None, 3, false),
            None,
            Some(db_path.as_path()),
        )
        .unwrap();

        // load_clusters resolved the sidecar and compute_affected_clusters found
        // every touched cluster (target + 4 dependents = 5).
        assert_eq!(
            result.affected_clusters.len(),
            5,
            "all five touched clusters must be reported; got: {:?}",
            result
                .affected_clusters
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>()
        );
        // 4 affected symbols is base Low, but >3 clusters touched applies the
        // cluster risk boost, escalating to Medium.
        assert_eq!(
            result.risk_level,
            RiskLevel::Medium,
            "the >3-cluster boost must escalate Low → Medium end-to-end"
        );
    }

    // ── silent-empty degradation is loud (not-indexed surfacing) ─────────

    /// Trust-core guarantee: when `list_repos` is empty but symbols exist under
    /// a `repo_uid`, the result must NOT read as a clean, fully-covered run.
    /// The owning repo is surfaced in `coverage.repos_not_indexed` and
    /// `blind_spots` names `NotIndexed`, so "not indexed" is always loud rather
    /// than silently collapsing to a low-risk empty answer.
    #[test]
    fn unindexed_owning_repo_is_surfaced_not_silent() {
        use nestweaver_schema::{Symbol, SymbolKind, Visibility};

        let store = GraphStore::in_memory().expect("in_memory store");
        // A symbol owned by repo:1 exists, but NO Repo row was inserted, so
        // `list_repos` returns empty and repo:1 is "not indexed" as metadata.
        store
            .insert_symbol(&Symbol {
                uid: "sym:a".to_string(),
                name: "fn_a".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:1".to_string(),
                file_path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn fn_a()".to_string(),
                summary: None,
                content_hash: "h_a".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();

        let result = analyze_blast_radius(
            &store,
            &[PathBuf::from("src/a.rs")],
            &opts(None, 3, false),
            None,
            None,
        )
        .unwrap();

        // The changed symbol resolved, but its owning repo is not in the repo
        // metadata — that gap is reported, never swallowed.
        assert_eq!(result.changed_symbols.len(), 1);
        assert!(
            result
                .coverage
                .repos_not_indexed
                .contains(&"repo:1".to_string()),
            "the owning repo absent from list_repos must appear in repos_not_indexed, got: {:?}",
            result.coverage.repos_not_indexed
        );
        assert!(
            result.blind_spots.contains(&BlindSpot::NotIndexed),
            "an unindexed owning repo must flag NotIndexed, got: {:?}",
            result.blind_spots
        );
    }
}
