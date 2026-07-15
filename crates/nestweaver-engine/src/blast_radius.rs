// PR blast radius analysis: maps changed files to affected symbols,
// runs transitive impact analysis, groups by cluster, and scores risk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::GraphStore;

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
    /// Confidence-weighted impact score (1.0 = direct high-confidence edge,
    /// decays multiplicatively through the graph). Used for sorting results
    /// so the most-affected symbols appear first.
    pub impact_score: f64,
    /// The repo_uid that owns this symbol (from the graph store).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo_uid: String,
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
    pub affected_name: String,
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
    fn label(self) -> &'static str {
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

/// Full result of a blast radius analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlastRadiusResult {
    pub changed_symbols: Vec<ChangedSymbol>,
    pub affected_symbols: Vec<AffectedSymbol>,
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
}

/// Derive the gate verdict from the run status and computed risk.
///
/// NON-NEGOTIABLE rule: a run that did not complete is never `RiskFlagged`
/// (we cannot trust an incomplete traversal to have found the risk), it is
/// `DegradedUnknown`.
fn derive_gate_state(status: AnalysisStatus, risk_level: RiskLevel) -> GateState {
    if status != AnalysisStatus::Complete {
        GateState::DegradedUnknown
    } else if matches!(risk_level, RiskLevel::High) {
        GateState::RiskFlagged
    } else {
        GateState::Ok
    }
}

/// Analyze the blast radius of a set of changed files.
///
/// 1. Maps changed files to their symbols in the graph.
/// 2. For each symbol, runs transitive impact analysis (CALLS, IMPORTS,
///    EXTENDS, IMPLEMENTS edges) up to `max_depth`.
/// 3. Groups affected symbols by cluster/community (if cluster data exists).
/// 4. Scores risk based on: number of affected symbols, PageRank centrality
///    of changed symbols, and number of clusters touched.
///
/// Risk levels:
/// - Low: <10 affected symbols
/// - Medium: 10-50 affected symbols
/// - High: 50-200 affected symbols
/// - Critical: >200 affected symbols (mapped to High since RiskLevel has 3 variants)
pub fn analyze_blast_radius(
    store: &GraphStore,
    changed_files: &[PathBuf],
    target_repo: Option<&str>,
    max_depth: u32,
    db_path: Option<&Path>,
) -> Result<BlastRadiusResult> {
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

        // A successful lookup that resolves 0 symbols is suspicious *only* when
        // (a) we scoped to a repo the graph actually knows, and (b) the file is
        // a recognized source file we would expect to have indexed. That points
        // at path drift or a not-yet-reindexed source file. We deliberately stay
        // silent for non-source files (docs, config, assets, lockfiles) — which
        // most PRs touch — and when there is no target repo to judge against, so
        // a healthy change never gates as degraded on benign files.
        if syms.is_empty() {
            if let Some(repo) = target_repo
                && known_repo_uids.contains(repo)
                && nestweaver_parser::detect_language(file).is_some()
            {
                notifications.push(Notification {
                    level: NotificationLevel::Warning,
                    message: format!(
                        "changed file {file_str} mapped to 0 symbols (possible path-format \
                         drift or unindexed file)"
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

    // Hard failure: we had files to analyze and *every* lookup errored. The
    // store is broken, not merely partial — a consumer must not read this as
    // "nothing affected".
    if !changed_files.is_empty() && files_ok == 0 && files_errored > 0 {
        status = status.max(AnalysisStatus::Failed);
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

    for cs in &changed_symbols {
        let impact_nodes = match store.impact(&cs.uid, max_depth, 0.0) {
            Ok(nodes) => nodes,
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
            if affected_uids.insert(node.uid.clone()) {
                // Look up the symbol so we know its kind and owning repo.
                let affected_sym = match store.lookup_symbol(&node.uid) {
                    Ok(sym) => Some(sym),
                    Err(_) => {
                        lookup_failures += 1;
                        None
                    }
                };
                let kind = affected_sym
                    .as_ref()
                    .map(|s| s.kind.to_string())
                    .unwrap_or_default();
                let affected_repo = affected_sym
                    .as_ref()
                    .map(|s| s.repo_uid.clone())
                    .unwrap_or_default();

                // If the affected symbol lives in a different repo than any
                // changed symbol, it is a cross-repo (org-wide) impact.
                if !affected_repo.is_empty() && !changed_repos.contains(&affected_repo) {
                    impacted_repos.insert(affected_repo.clone());
                    let repo_label = repo_display
                        .get(&affected_repo)
                        .cloned()
                        .unwrap_or_else(|| affected_repo.clone());
                    let item = OrgImpactItem {
                        change_name: cs.name.clone(),
                        change_kind: cs.kind.clone(),
                        affected_name: node.name.clone(),
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
                    impact_score: node.impact_score,
                    repo_uid: affected_repo.clone(),
                });
            }
        }
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

    // Step 4: Score risk.
    let total_affected = affected_symbols.len();
    let clusters_touched = affected_clusters.len();

    // Factor in PageRank centrality: if high-centrality symbols are changed,
    // bump the risk. Average PageRank of changed symbols; a score > 0.01
    // is considered "high centrality" in a typical graph.
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

    let risk_level = compute_risk_level(total_affected, clusters_touched, avg_pagerank);

    let mut summary = format!(
        "{} changed symbol(s) in {} file(s), {} transitively affected symbol(s), \
         {} cluster(s) touched. Risk: {:?}.",
        changed_symbols.len(),
        changed_files.len(),
        affected_symbols.len(),
        clusters_touched,
        risk_level,
    );
    // Make a non-clean run impossible to miss in the human summary.
    if status != AnalysisStatus::Complete {
        summary.push_str(&format!(" [status: {}]", status.label()));
    }

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

    Ok(BlastRadiusResult {
        changed_symbols,
        affected_symbols,
        affected_clusters,
        risk_level,
        summary,
        org_wide,
        status,
        notifications,
        gate_state,
    })
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
fn compute_risk_level(
    affected_count: usize,
    clusters_touched: usize,
    avg_pagerank: f64,
) -> RiskLevel {
    // Base risk from affected symbol count:
    //   <10 = Low (0), 10-50 = Medium (1), 50-200 = High (2), >200 = High (3)
    let base = match affected_count {
        0..10 => 0,
        10..50 => 1,
        50..200 => 2,
        _ => 3,
    };

    // Boost for high-centrality symbols (avg PageRank > 0.01).
    let centrality_boost = if avg_pagerank > 0.01 { 1 } else { 0 };

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

    #[test]
    fn compute_risk_level_low() {
        assert_eq!(compute_risk_level(5, 1, 0.001), RiskLevel::Low);
    }

    #[test]
    fn compute_risk_level_medium_by_count() {
        assert_eq!(compute_risk_level(25, 1, 0.001), RiskLevel::Medium);
    }

    #[test]
    fn compute_risk_level_high_by_count() {
        assert_eq!(compute_risk_level(100, 1, 0.001), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_critical_by_count() {
        assert_eq!(compute_risk_level(300, 1, 0.001), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_boosted_by_centrality() {
        // 25 affected would be Medium, but high centrality bumps it to High
        assert_eq!(compute_risk_level(25, 1, 0.05), RiskLevel::High);
    }

    #[test]
    fn compute_risk_level_boosted_by_clusters() {
        // 25 affected would be Medium, but >3 clusters bumps it to High
        assert_eq!(compute_risk_level(25, 5, 0.001), RiskLevel::High);
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
        let result =
            analyze_blast_radius(&store, &[PathBuf::from("nonexistent.rs")], None, 3, None).unwrap();
        assert!(result.changed_symbols.is_empty());
        assert!(result.affected_symbols.is_empty());
        assert_eq!(result.risk_level, RiskLevel::Low);
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

        let result =
            analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], None, 3, None).unwrap();

        assert_eq!(result.changed_symbols.len(), 1);
        assert_eq!(result.changed_symbols[0].name, "fn_a");
        assert_eq!(result.affected_symbols.len(), 1);
        assert_eq!(result.affected_symbols[0].name, "fn_b");
        // fn_a has pagerank_score=0.5 (high centrality), which boosts
        // the risk from Low to Medium even with only 1 affected symbol.
        assert_eq!(result.risk_level, RiskLevel::Medium);

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

        let result =
            analyze_blast_radius(&store, &[PathBuf::from("src/api.rs")], None, 3, None).unwrap();

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

        let result =
            analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], None, 3, None).unwrap();
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

        let result =
            analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], None, 5, None).unwrap();

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

        let result =
            analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], None, 5, None).unwrap();

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
            Some("repo:1"),
            3,
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
        let unscoped =
            analyze_blast_radius(&store, &[PathBuf::from("src/main.rs")], None, 3, None).unwrap();
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

        let result =
            analyze_blast_radius(&store, &[PathBuf::from("src/a.rs")], None, 3, None).unwrap();

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
            Some("repo:1"),
            3,
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
        let result =
            analyze_blast_radius(&store, &[PathBuf::from("README.md")], Some("repo:1"), 3, None)
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
}
