//! Hierarchical code summaries for token-efficient retrieval.
//!
//! Generates deterministic, compact summaries at three levels of the code graph:
//!
//! - **Symbol**: one-line summary of a function/class with callers and callees
//! - **File**: list of exports and their kinds, plus import sources
//! - **Cluster**: community-level description with key types and dependencies
//!
//! These summaries are generated entirely from graph data — no LLM needed.
//! They are designed to give an LLM maximum architectural understanding per
//! token, following the HCGS (Hierarchical Code Graph Summarization) approach.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::GraphStore;

// ── Types ────────────────────────────────────────────────────────────────────

/// The granularity of a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SummaryLevel {
    Symbol,
    File,
    Cluster,
    /// Top hub nodes with a concise structural (call-graph shape) summary.
    Hub,
}

impl std::fmt::Display for SummaryLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummaryLevel::Symbol => write!(f, "symbol"),
            SummaryLevel::File => write!(f, "file"),
            SummaryLevel::Cluster => write!(f, "cluster"),
            SummaryLevel::Hub => write!(f, "hub"),
        }
    }
}

impl std::str::FromStr for SummaryLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "symbol" => Ok(SummaryLevel::Symbol),
            "file" => Ok(SummaryLevel::File),
            "cluster" => Ok(SummaryLevel::Cluster),
            "hub" => Ok(SummaryLevel::Hub),
            other => Err(format!(
                "unknown summary level '{}': expected symbol, file, cluster, or hub",
                other
            )),
        }
    }
}

/// A single deterministic summary entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub level: SummaryLevel,
    pub target_uid: String,
    pub target_name: String,
    pub content: String,
    /// Estimated token count (chars / 4).
    pub token_estimate: usize,
    /// Source file path of the summarized target. Currently populated only
    /// for `Hub` summaries (so consumers like the agent guide can render the
    /// path without re-parsing `content`); `None` for other levels and for
    /// sidecars written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
}

/// Persisted collection of summaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryStore {
    pub summaries: Vec<Summary>,
    /// The graph generation these summaries were built from. Loading checks this
    /// against the store's current generation and treats a mismatch as a miss,
    /// so a reindex never serves stale summaries. Defaults to 0 for
    /// pre-generation sidecars (which are then always treated as stale).
    #[serde(default)]
    pub graph_generation: u64,
    /// nw-361. How many rows the GENERATOR dropped, per level, when this set
    /// was written — keyed by [`SummaryLevel`]'s `Display` form.
    ///
    /// The cap used to be a property of the CODE PATH that produced the set
    /// rather than of the STORED set, so on a sidecar cache hit the generator
    /// did not run, nothing recorded what it had dropped, and a set that WAS
    /// capped when it was written read back as complete.
    ///
    /// `Option`, not a bare map: `None` means "the producer did not record
    /// this", which is NOT the same claim as "nothing was dropped". An empty
    /// map is a positive statement that this writer looked and found no cap;
    /// absence is the absence of a statement. [`load_summaries_with_cap`] is
    /// where that distinction is enforced.
    #[serde(default)]
    pub cap_dropped: Option<std::collections::BTreeMap<String, usize>>,
}

/// Build the one-level provenance map a writer that regenerated exactly one
/// level should persist.
///
/// A helper rather than a `BTreeMap::from` at each call site so the KEY
/// spelling (`SummaryLevel`'s `Display`) is decided once — a second spelling
/// is how the writer and the reader would come to disagree about which level
/// a count belongs to.
pub fn cap_provenance(
    level: SummaryLevel,
    dropped: usize,
) -> std::collections::BTreeMap<String, usize> {
    let mut map = std::collections::BTreeMap::new();
    map.insert(level.to_string(), dropped);
    map
}

// ── Generation ───────────────────────────────────────────────────────────────

/// Generate summaries at the given level from the graph store.
pub fn generate_summaries(store: &GraphStore, level: SummaryLevel) -> Result<Vec<Summary>> {
    match level {
        // Untargeted symbol-level generation is bounded so it can't hang on a
        // large graph; callers wanting a specific symbol should use
        // `generate_symbol_summaries_bounded` with a target for the fast path.
        SummaryLevel::Symbol => {
            generate_symbol_summaries_bounded(store, None, DEFAULT_SYMBOL_SUMMARY_CAP)
                .map(|r| r.summaries)
        }
        SummaryLevel::File => generate_file_summaries(store),
        SummaryLevel::Cluster => generate_cluster_summaries(store),
        SummaryLevel::Hub => generate_hub_summaries(store),
    }
}

/// Coarse architectural role for a hub from its caller/callee degree balance.
fn hub_role(in_degree: usize, out_degree: usize) -> &'static str {
    if in_degree + out_degree == 0 {
        "isolated"
    } else if in_degree >= out_degree.saturating_mul(3) {
        // Heavily called, calls little → a shared sink/utility.
        "sink/utility"
    } else if out_degree >= in_degree.saturating_mul(3) {
        // Calls widely, called little → an orchestrator/entry point.
        "orchestrator"
    } else {
        // High degree both ways → a bridge/coordinator.
        "bridge"
    }
}

/// Hub-level: one concise structural summary per top hub node — its call-graph
/// shape (in/out degree) plus a coarse architectural role. Precomputed and
/// cached (generation-gated via the summary sidecar), so `get_summary --level
/// hub` gives architectural orientation without the ~500ms live hub scan.
fn generate_hub_summaries(store: &GraphStore) -> Result<Vec<Summary>> {
    generate_hub_summaries_bounded(store).map(|out| out.summaries)
}

/// Hub summaries with the two facts that say whether the list is complete.
pub struct HubSummaries {
    pub summaries: Vec<Summary>,
    /// Symbols with at least one code edge — the population hubs are drawn from.
    pub matched_total: usize,
    pub capped: bool,
}

/// Hub summaries plus the candidate count they were selected from.
///
/// Same finding as [`generate_cluster_summaries_bounded`], one level down, and
/// found by sweeping the other three `SummaryLevel`s rather than by a report:
/// `HUB_COUNT` is an INTERNAL constant the caller never stated, so reporting
/// `total: 30` is a claim that the graph HAS thirty hubs. Measured on a
/// 60-module fixture with 180 edge-bearing symbols, `summary --level hub`
/// answered `{returned: 30, total: 30, truncated: false}`.
///
/// `Commands::Hubs` is deliberately NOT changed: there `--top N` is the
/// caller's own request, and returning N of N is the correct answer to it.
/// The lie is specific to a bound the caller could not see or state.
pub fn generate_hub_summaries_bounded(store: &GraphStore) -> Result<HubSummaries> {
    const HUB_COUNT: usize = 30;
    let found = crate::hubs::find_hub_nodes_bounded(store, HUB_COUNT)?;
    let candidate_total = found.candidate_total;
    let summaries: Vec<Summary> = found
        .hubs
        .iter()
        .map(|h| {
            let role = hub_role(h.in_degree, h.out_degree);
            let content = format!(
                "{} ({}) — hub [{}]: {} callers, {} callees (total degree {})",
                h.name, h.file_path, role, h.in_degree, h.out_degree, h.total_degree
            );
            let token_estimate = content.len() / 4;
            Summary {
                level: SummaryLevel::Hub,
                target_uid: h.uid.clone(),
                target_name: h.name.clone(),
                content,
                token_estimate,
                file_path: Some(h.file_path.clone()),
            }
        })
        .collect();
    Ok(HubSummaries {
        capped: candidate_total > summaries.len(),
        matched_total: candidate_total,
        summaries,
    })
}

/// Default cap on how many symbols an *untargeted* symbol-level summary will
/// process. Each symbol costs ~4 graph queries (callers + 3 callee edge types),
/// so an unbounded scan over a large graph hangs for tens of seconds (~11s even
/// at 1000). This path is the one the tool description steers away from ("use
/// target"), so keep the ceiling low enough to stay responsive and rely on the
/// `partial`/note signal to point callers at a `target`. A targeted call bypasses
/// the cap entirely since it only touches matches.
pub const DEFAULT_SYMBOL_SUMMARY_CAP: usize = 500;

/// Result of a bounded / target-filtered symbol-summary generation.
pub struct SymbolSummaries {
    pub summaries: Vec<Summary>,
    /// Symbols that matched the `target` filter (or the whole store when no
    /// target) BEFORE the cap — so callers can report honest truncation.
    pub matched_total: usize,
    /// True when the untargeted cap dropped some matches (`matched_total` >
    /// `summaries.len()`).
    pub capped: bool,
}

/// Symbol-level summaries with the `target` filter pushed down before batched
/// caller/callee retrieval, and a hard cap so an untargeted call over a large
/// graph can't hang.
///
/// - `target = Some(t)`: only symbols whose name or uid contains `t`
///   (case-insensitive) are summarized. The cap does not apply — a targeted
///   query is inherently small — so `capped` is always false.
/// - `target = None`: at most `cap` symbols (deterministic by uid) are
///   summarized; `capped` is set when more exist. This is the pathological case
///   the tool description already steers away from ("use target to filter").
pub fn generate_symbol_summaries_bounded(
    store: &GraphStore,
    target: Option<&str>,
    cap: usize,
) -> Result<SymbolSummaries> {
    let mut symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("list_all_symbols: {e}"))?;

    // Push the target filter down: match name/file-path substring or exact UID
    // BEFORE any of the per-symbol caller/callee queries, mirroring
    // `filter_by_target` so the two stay consistent (UIDs embed hex hashes, so
    // a UID *substring* match would retain virtually every symbol). This turns
    // the common single-symbol lookup into O(matches) graph queries instead of
    // O(all symbols).
    if let Some(t) = target {
        let needle = t.to_lowercase();
        symbols.retain(|s| {
            s.name.to_lowercase().contains(&needle)
                || s.file_path.to_lowercase().contains(&needle)
                || s.uid.eq_ignore_ascii_case(t)
        });
    }

    let matched_total = symbols.len();

    // Untargeted: bound the work. Sort by uid so the retained subset is
    // deterministic across runs, then cap.
    let mut capped = false;
    if target.is_none() && symbols.len() > cap {
        symbols.sort_by(|a, b| a.uid.cmp(&b.uid));
        symbols.truncate(cap);
        capped = true;
    }

    let selected_uids = symbols
        .iter()
        .map(|symbol| symbol.uid.clone())
        .collect::<Vec<_>>();
    let adjacency = store
        .summary_adjacency_by_uid(&selected_uids)
        .map_err(|error| anyhow::anyhow!("summary adjacency batch: {error}"))?;
    let mut summaries = Vec::with_capacity(symbols.len());

    for sym in &symbols {
        let (caller_names, callee_names) = adjacency
            .get(&sym.uid)
            .map(|(callers, callees)| (callers.as_slice(), callees.as_slice()))
            .unwrap_or_default();

        let content = format!(
            "{kind} {sig} | callers: [{callers}] | callees: [{callees}] | file: {file}:{line}",
            kind = sym.kind,
            sig = sym.signature,
            callers = caller_names.join(", "),
            callees = callee_names.join(", "),
            file = sym.file_path,
            line = sym.start_line,
        );

        let token_estimate = content.len().div_ceil(4);

        summaries.push(Summary {
            level: SummaryLevel::Symbol,
            target_uid: sym.uid.clone(),
            target_name: sym.name.clone(),
            content,
            token_estimate,
            file_path: None,
        });
    }

    // Sort by content string for deterministic output.
    summaries.sort_by(|a, b| a.content.cmp(&b.content));

    Ok(SymbolSummaries {
        summaries,
        matched_total,
        capped,
    })
}

/// File-level: exports and import sources per file.
///
/// Format: `{file}: exports {n} symbols: {name1} ({kind}), ... | imports from: [files]`
fn generate_file_summaries(store: &GraphStore) -> Result<Vec<Summary>> {
    let symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("list_all_symbols: {e}"))?;

    // Group symbols by file path.
    let mut by_file: BTreeMap<String, Vec<&nestweaver_schema::Symbol>> = BTreeMap::new();
    for sym in &symbols {
        by_file.entry(sym.file_path.clone()).or_default().push(sym);
    }

    // Build a set of file paths that each file imports from, by checking
    // CALLS edges. If symbol A in file X calls symbol B in file Y, then
    // file X imports from file Y.
    let (_, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!("load_code_symbols_and_edges: {e}"))?;

    // Build uid -> file_path map.
    let uid_to_file: HashMap<&str, &str> = symbols
        .iter()
        .map(|s| (s.uid.as_str(), s.file_path.as_str()))
        .collect();

    // file_path -> set of file_paths it depends on.
    let mut imports_from: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (src_uid, dst_uid, _) in &edges {
        if let (Some(&src_file), Some(&dst_file)) = (
            uid_to_file.get(src_uid.as_str()),
            uid_to_file.get(dst_uid.as_str()),
        ) && src_file != dst_file
        {
            imports_from.entry(src_file).or_default().insert(dst_file);
        }
    }

    let mut summaries = Vec::with_capacity(by_file.len());

    for (file_path, file_symbols) in &by_file {
        // Sort symbols by start_line for deterministic output.
        let mut sorted_syms: Vec<&&nestweaver_schema::Symbol> = file_symbols.iter().collect();
        sorted_syms.sort_by_key(|s| s.start_line);

        let export_list: Vec<String> = sorted_syms
            .iter()
            .map(|s| format!("{} ({})", s.name, s.kind))
            .collect();

        let import_files: Vec<&str> = imports_from
            .get(file_path.as_str())
            .map(|set| {
                let mut v: Vec<&str> = set.iter().copied().collect();
                v.sort();
                v
            })
            .unwrap_or_default();

        let content = format!(
            "{file}: exports {n} symbols: {exports} | imports from: [{imports}]",
            file = file_path,
            n = file_symbols.len(),
            exports = export_list.join(", "),
            imports = import_files.join(", "),
        );

        let token_estimate = content.len().div_ceil(4);

        // Use file path as both UID and name (files don't have a separate UID
        // in the same sense symbols do — we use the path as the identity key).
        summaries.push(Summary {
            level: SummaryLevel::File,
            target_uid: file_path.clone(),
            target_name: file_path.clone(),
            content,
            token_estimate,
            file_path: None,
        });
    }

    Ok(summaries)
}

/// Maximum number of clusters to summarize. On large graphs, local moving can
/// produce thousands of tiny communities; summarizing all of them yields
/// noise rather than signal. We keep only the top-N largest clusters.
pub const MAX_CLUSTER_SUMMARIES: usize = 50;

/// Lower resolution parameter used when the default (1.0) produces too
/// many small clusters. A lower resolution merges communities more
/// aggressively, yielding fewer but larger clusters.
const FALLBACK_CLUSTER_RESOLUTION: f64 = 0.5;

/// Cluster-level: community description with key types and cross-cluster deps.
///
/// Format: `Cluster {id} ({name}, {n} symbols): key types: [{top symbols}] | files: [{file list}] | depends on: [other clusters]`
///
/// At scale (tens of thousands of symbols), local moving with resolution=1.0
/// can produce thousands of singleton or near-singleton communities that are not
/// useful for summarization. This function:
/// 1. Filters out singleton clusters (1 member).
/// 2. If the default resolution produces no non-singleton clusters, retries
///    with a lower resolution to force larger communities.
/// 3. Limits output to the top-N largest clusters by member count.
fn generate_cluster_summaries(store: &GraphStore) -> Result<Vec<Summary>> {
    generate_cluster_summaries_bounded(store, MAX_CLUSTER_SUMMARIES).map(|out| out.summaries)
}

/// Cluster summaries plus the two facts a caller needs to know it is looking
/// at a capped view.
pub struct ClusterSummaries {
    pub summaries: Vec<Summary>,
    /// How many non-singleton communities MATCHED, before the cap.
    pub matched_total: usize,
    /// Whether the cap dropped any.
    pub capped: bool,
}

/// F-DC-11 (folded into nw-299). `generate_cluster_summaries` truncated to 50
/// and returned a bare `Vec`, which has nowhere to say "I dropped some". The
/// caller then computed `total` from the already-capped vector, so
/// `summary --level cluster` reported `{returned: 50, total: 50,
/// truncated: false}` against a 71,184-community graph. The honesty machinery
/// existed — `generate_symbol_summaries_bounded` returns exactly this pair —
/// and was wired for `SummaryLevel::Symbol` only.
pub fn generate_cluster_summaries_bounded(
    store: &GraphStore,
    cap: usize,
) -> Result<ClusterSummaries> {
    // F-DC-7: the SAME adaptive default the `clusters` tool and the
    // `clusters`/`cluster` CLI commands use. This used to be a hard-coded 1.0,
    // which put the IDs in this summary into a different partition from the
    // one `cluster <id>` resolves against.
    let mut output = crate::cluster_dispatch::compute_clusters(
        store,
        crate::cluster_dispatch::default_cluster_resolution(store),
    )?;

    // Filter out singleton clusters — they carry no architectural signal.
    output.communities.retain(|c| c.member_count > 1);

    // If the default resolution produced no usable clusters (everything was a
    // singleton), retry with a lower resolution that merges more aggressively.
    if output.communities.is_empty() {
        output = crate::cluster_dispatch::compute_clusters(store, FALLBACK_CLUSTER_RESOLUTION)?;
        output.communities.retain(|c| c.member_count > 1);
    }

    if output.communities.is_empty() {
        return Ok(ClusterSummaries {
            summaries: vec![],
            matched_total: 0,
            capped: false,
        });
    }

    // Captured BEFORE the cap. Computing it after is the whole of F-DC-11.
    let matched_total = output.communities.len();
    let capped = matched_total > cap;

    // Communities are already sorted by size descending from compute_clusters.
    // Keep only the top-N largest for summarization.
    output.communities.truncate(cap);

    // Build symbol UID -> cluster ID mapping for cross-cluster dependency detection.
    let mut uid_to_cluster: HashMap<String, u32> = HashMap::new();
    for community in &output.communities {
        for member in &community.members {
            uid_to_cluster.insert(member.uid.clone(), community.id);
        }
    }

    // Load edges to find cross-cluster dependencies.
    let (_, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!("load_code_symbols_and_edges: {e}"))?;

    // cluster_id -> set of cluster_ids it depends on (outgoing edges to other clusters).
    let mut cluster_deps: HashMap<u32, HashSet<u32>> = HashMap::new();
    for (src_uid, dst_uid, _) in &edges {
        if let (Some(&src_cluster), Some(&dst_cluster)) =
            (uid_to_cluster.get(src_uid), uid_to_cluster.get(dst_uid))
            && src_cluster != dst_cluster
        {
            cluster_deps
                .entry(src_cluster)
                .or_default()
                .insert(dst_cluster);
        }
    }

    // Build a name lookup: cluster_id -> cluster_name.
    let id_to_name: HashMap<u32, &str> = output
        .communities
        .iter()
        .map(|c| (c.id, c.name.as_str()))
        .collect();

    let mut summaries = Vec::with_capacity(output.communities.len());

    for community in &output.communities {
        // Key types: top 5 symbols by PageRank (or just the first 5 alphabetically).
        let mut key_members = community.members.clone();
        key_members.truncate(5);
        let key_types: Vec<String> = key_members
            .iter()
            .map(|m| format!("{} ({})", m.name, m.kind))
            .collect();

        // Dependent cluster names.
        let dep_names: Vec<String> = cluster_deps
            .get(&community.id)
            .map(|deps| {
                let mut names: Vec<String> = deps
                    .iter()
                    .filter_map(|id| id_to_name.get(id).map(|n| n.to_string()))
                    .collect();
                names.sort();
                names
            })
            .unwrap_or_default();

        let content = format!(
            "Cluster {id} ({name}, {n} symbols): key types: [{keys}] | files: [{files}] | depends on: [{deps}]",
            id = community.id,
            name = community.name,
            n = community.member_count,
            keys = key_types.join(", "),
            files = community.key_files.join(", "),
            deps = dep_names.join(", "),
        );

        let token_estimate = content.len().div_ceil(4);

        summaries.push(Summary {
            level: SummaryLevel::Cluster,
            target_uid: format!("cluster:{}", community.id),
            target_name: community.name.clone(),
            content,
            token_estimate,
            file_path: None,
        });
    }

    Ok(ClusterSummaries {
        summaries,
        matched_total,
        capped,
    })
}

// ── Sidecar persistence ──────────────────────────────────────────────────────

/// Compute the sidecar file path: `<db>.summaries.json`.
///
/// Uses `OsStr::push` to preserve the `.lbug` extension, producing e.g.
/// `test.lbug.summaries.json`.
pub fn sidecar_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".summaries.json")
}

/// Save summaries to the sidecar file.
/// nw-361: `cap_dropped` is REQUIRED, not optional, because every writer in
/// the tree knows the answer at the point it writes — the generators all
/// return `matched_total` beside the summaries. Making it a parameter is what
/// stops a fourth writer being added that silently claims completeness.
pub fn save_summaries(
    db_path: &Path,
    graph_generation: u64,
    summaries: &[Summary],
    cap_dropped: &std::collections::BTreeMap<String, usize>,
) -> Result<()> {
    let path = sidecar_path(db_path);
    let store = SummaryStore {
        summaries: summaries.to_vec(),
        graph_generation,
        cap_dropped: Some(cap_dropped.clone()),
    };
    let json = serde_json::to_string_pretty(&store).context("failed to serialize summaries")?;
    fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Warm the sidecar with freshly generated `fresh` summaries for `level`,
/// preserving any cached entries at OTHER levels, then persist. Best-effort:
/// a load miss/mismatch starts from empty and a save error is logged and
/// swallowed so warming the cache never fails the caller.
///
/// Both `get_summary` (MCP) and the agent-guide Architecture section warm the
/// cache after a cold read; centralizing the "keep other levels, replace this
/// level, save" invariant here keeps the two from drifting.
pub fn merge_and_save_summaries(
    db_path: &Path,
    generation: u64,
    level: SummaryLevel,
    fresh: &[Summary],
    cap_dropped: usize,
) {
    // nw-361: the per-level drop map is merged HERE and nowhere else, because
    // this function already owns the "keep other levels, replace this level"
    // invariant. A second place that merged it is how the summaries and their
    // provenance would come to describe different sets.
    let existing = load_summary_store(db_path, generation).ok().flatten();
    let mut all: Vec<Summary> = match existing {
        Some(ref store) => store
            .summaries
            .iter()
            .filter(|s| s.level != level)
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let mut drops = existing.and_then(|s| s.cap_dropped).unwrap_or_default();
    // Retain only the levels we are actually carrying forward; a count for a
    // level whose summaries we just dropped would outlive its set.
    drops.retain(|k, _| all.iter().any(|s| s.level.to_string() == *k));
    drops.insert(level.to_string(), cap_dropped);
    all.extend(fresh.iter().cloned());
    if let Err(e) = save_summaries(db_path, generation, &all, &drops) {
        tracing::warn!("failed to warm summaries sidecar (level {level:?}): {e}");
    }
}

/// Load summaries from the sidecar file. Returns `Ok(None)` when the sidecar is
/// missing OR was built from a different graph generation than
/// `expected_generation` — a mismatch is treated as a miss so a reindex never
/// serves stale summaries (the caller regenerates).
pub fn load_summaries(db_path: &Path, expected_generation: u64) -> Result<Option<Vec<Summary>>> {
    Ok(load_summary_store(db_path, expected_generation)?.map(|s| s.summaries))
}

/// The generation-gated sidecar read both public loaders are built on.
fn load_summary_store(db_path: &Path, expected_generation: u64) -> Result<Option<SummaryStore>> {
    crate::migrate_sidecar(db_path, "summaries.json", ".summaries.json");
    let path = sidecar_path(db_path);
    if !path.exists() {
        return Ok(None);
    }
    let json =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let store: SummaryStore =
        serde_json::from_str(&json).context("failed to parse summaries sidecar")?;
    if store.graph_generation != expected_generation {
        return Ok(None);
    }
    Ok(Some(store))
}

/// Load summaries for a caller that will make a TRUNCATION CLAIM about them,
/// returning the per-level generator drop counts alongside.
///
/// nw-361. `load_summaries` is the plain sibling, for callers that claim
/// nothing — the shape `collect_tolerating_corrupt` established. This one is
/// for `get_summary`, which publishes `truncated` and `truncated_by_cap`.
///
/// **A sidecar with no recorded provenance is reported as a MISS.** Absent
/// must not read as `false`: a set written by a binary older than
/// [`SummaryStore::cap_dropped`] cannot say what its generator dropped, and
/// serving it would republish exactly the silent-completeness claim this
/// exists to remove. The caller then regenerates and records — one extra
/// generation, once per database, which is the same trade `.filemeta.json`'s
/// version gate makes for the same reason. Returning a tri-state to the wire
/// instead was rejected: it fixes `truncated_by_cap` and leaves `truncated`
/// itself still under-reporting, because the count it needs is the thing that
/// is missing.
pub fn load_summaries_with_cap(
    db_path: &Path,
    expected_generation: u64,
) -> Result<Option<(Vec<Summary>, std::collections::BTreeMap<String, usize>)>> {
    let Some(store) = load_summary_store(db_path, expected_generation)? else {
        return Ok(None);
    };
    match store.cap_dropped {
        Some(drops) => Ok(Some((store.summaries, drops))),
        None => Ok(None),
    }
}

// ── Token-budget truncation ──────────────────────────────────────────────────

/// Truncate a list of summaries to fit within a token budget.
///
/// Returns the subset of summaries that fits, in order.
/// Approximate token cap `summary` applies when the caller names none.
///
/// ONE constant, shared by the CLI flag and the `get_summary` MCP tool. nw-182
/// bounded this on the CLI because `summary --level file --json` emitted 8.3 MB
/// — output proportional to the corpus rather than to the question — and the
/// reason recorded for fixing it was, verbatim, "this is exposed as an MCP tool
/// where that is a context-window bomb". The cap was then applied only to the
/// CLI, so the surface the fix was FOR kept emitting the 8.3 MB.
///
/// `0` still means unlimited on both surfaces.
pub const SUMMARY_DEFAULT_TOKEN_BUDGET: usize = 20_000;

pub fn truncate_to_budget(summaries: &[Summary], token_budget: usize) -> Vec<&Summary> {
    let mut used = 0usize;
    let mut result = Vec::new();
    for s in summaries {
        if used + s.token_estimate > token_budget {
            break;
        }
        used += s.token_estimate;
        result.push(s);
    }
    result
}

/// Filter summaries by target name or file path (substring, case-insensitive),
/// or by exact UID.
///
/// The UID is matched exactly, never as a substring: symbol UIDs embed hex
/// hashes (`sym:repo:default:c8f000561246:6a4df1030d93:…`), so a one-character
/// target like `b` substring-matches virtually every UID in the graph and the
/// filter returns everything.
pub fn filter_by_target<'a>(summaries: &'a [Summary], target: &str) -> Vec<&'a Summary> {
    let needle = target.to_lowercase();
    summaries
        .iter()
        .filter(|s| {
            s.target_name.to_lowercase().contains(&needle)
                || s.file_path
                    .as_deref()
                    .is_some_and(|fp| fp.to_lowercase().contains(&needle))
                || s.target_uid.eq_ignore_ascii_case(target)
        })
        .collect()
}

/// Render summaries as plain text, one per line.
pub fn render_text(summaries: &[Summary]) -> String {
    summaries
        .iter()
        .map(|s| s.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_level_roundtrip() {
        for level in [
            SummaryLevel::Symbol,
            SummaryLevel::File,
            SummaryLevel::Cluster,
        ] {
            let s = level.to_string();
            let parsed: SummaryLevel = s.parse().unwrap();
            assert_eq!(parsed, level);
        }
    }

    #[test]
    fn summary_level_parse_error() {
        let result: Result<SummaryLevel, _> = "invalid".parse();
        assert!(result.is_err());
    }

    #[test]
    fn truncate_to_budget_respects_limit() {
        let summaries = vec![
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "a".to_string(),
                target_name: "fn_a".to_string(),
                content: "x".repeat(40), // 10 tokens
                token_estimate: 10,
                file_path: None,
            },
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "b".to_string(),
                target_name: "fn_b".to_string(),
                content: "y".repeat(40),
                token_estimate: 10,
                file_path: None,
            },
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "c".to_string(),
                target_name: "fn_c".to_string(),
                content: "z".repeat(40),
                token_estimate: 10,
                file_path: None,
            },
        ];

        let result = truncate_to_budget(&summaries, 25);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].target_uid, "a");
        assert_eq!(result[1].target_uid, "b");
    }

    #[test]
    fn truncate_to_budget_empty_budget() {
        let summaries = vec![Summary {
            level: SummaryLevel::Symbol,
            target_uid: "a".to_string(),
            target_name: "fn_a".to_string(),
            content: "x".repeat(40),
            token_estimate: 10,
            file_path: None,
        }];

        let result = truncate_to_budget(&summaries, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn filter_by_target_matches_name() {
        let summaries = vec![
            Summary {
                level: SummaryLevel::File,
                target_uid: "src/auth.rs".to_string(),
                target_name: "src/auth.rs".to_string(),
                content: "auth stuff".to_string(),
                token_estimate: 3,
                file_path: None,
            },
            Summary {
                level: SummaryLevel::File,
                target_uid: "src/main.rs".to_string(),
                target_name: "src/main.rs".to_string(),
                content: "main stuff".to_string(),
                token_estimate: 3,
                file_path: None,
            },
        ];

        let result = filter_by_target(&summaries, "auth");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target_name, "src/auth.rs");
    }

    #[test]
    fn filter_by_target_does_not_substring_match_uid_hash() {
        // Regression: symbol UIDs embed hex hashes, so a 1-char target like
        // "b" used to substring-match every UID and return the whole graph.
        let summaries = vec![
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "sym:repo:default:c8f000561246:6a4df1030d93:895572949b24:121"
                    .to_string(),
                target_name: "compute_score".to_string(),
                content: "score stuff".to_string(),
                token_estimate: 3,
                file_path: Some("src/score.rs".to_string()),
            },
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "sym:repo:default:c8f000561246:17b788a70eec:17b788a70eec:126"
                    .to_string(),
                target_name: "render".to_string(),
                content: "render stuff".to_string(),
                token_estimate: 3,
                file_path: Some("src/render.rs".to_string()),
            },
        ];

        // "b" appears in both UID hashes but in neither name nor file path.
        assert!(
            filter_by_target(&summaries, "b").is_empty(),
            "a hex-digit target must not match UID substrings"
        );

        // Name substring, file-path substring, and exact UID still match.
        assert_eq!(filter_by_target(&summaries, "render").len(), 1);
        assert_eq!(filter_by_target(&summaries, "score.rs").len(), 1);
        assert_eq!(
            filter_by_target(
                &summaries,
                "sym:repo:default:c8f000561246:6a4df1030d93:895572949b24:121"
            )
            .len(),
            1,
            "an exact UID must still target its summary"
        );
    }

    #[test]
    fn render_text_joins_lines() {
        let summaries = vec![
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "a".to_string(),
                target_name: "a".to_string(),
                content: "line one".to_string(),
                token_estimate: 2,
                file_path: None,
            },
            Summary {
                level: SummaryLevel::Symbol,
                target_uid: "b".to_string(),
                target_name: "b".to_string(),
                content: "line two".to_string(),
                token_estimate: 2,
                file_path: None,
            },
        ];

        let text = render_text(&summaries);
        assert_eq!(text, "line one\nline two");
    }

    #[test]
    fn symbol_summaries_from_store() {
        let store = GraphStore::in_memory().unwrap();
        let sym = nestweaver_schema::Symbol {
            uid: "sym:test:abc:10".to_string(),
            name: "greet".to_string(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: "src/main.js".to_string(),
            start_line: 10,
            end_line: 10,
            signature: "function greet(name)".to_string(),
            summary: None,
            content_hash: "abc".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&sym).unwrap();

        let out =
            generate_symbol_summaries_bounded(&store, None, DEFAULT_SYMBOL_SUMMARY_CAP).unwrap();
        let summaries = out.summaries;
        assert_eq!(summaries.len(), 1);
        assert!(!out.capped);
        assert_eq!(out.matched_total, 1);
        assert!(summaries[0].content.contains("greet"));
        assert!(summaries[0].content.contains("src/main.js:10"));
        assert_eq!(summaries[0].level, SummaryLevel::Symbol);
        assert!(summaries[0].token_estimate > 0);
    }

    #[test]
    fn symbol_summaries_batch_callers_and_callees_without_losing_semantics() {
        use nestweaver_schema::{EdgeType, ResolvedEdge};

        let store = GraphStore::in_memory().unwrap();
        let make_symbol = |uid: &str, name: &str| nestweaver_schema::Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: uid.to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        for symbol in [
            make_symbol("caller", "caller_fn"),
            make_symbol("target", "target_fn"),
            make_symbol("callee", "callee_fn"),
        ] {
            store.insert_symbol(&symbol).unwrap();
        }
        for (source_uid, target_uid) in [("caller", "target"), ("target", "callee")] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: source_uid.to_string(),
                    target_uid: target_uid.to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 1.0,
                    link_type: None,
                    evidence: Vec::new(),
                })
                .unwrap();
        }

        let output = generate_symbol_summaries_bounded(&store, Some("target_fn"), 10).unwrap();
        assert_eq!(output.summaries.len(), 1);
        let content = &output.summaries[0].content;
        assert!(content.contains("callers: [caller_fn]"), "{content}");
        assert!(content.contains("callees: [callee_fn]"), "{content}");
    }

    #[test]
    fn symbol_summaries_target_pushdown_and_cap() {
        // nw-079: a target filter returns only matching symbols (fast path), and
        // an untargeted call over more than `cap` symbols is bounded + flagged.
        let store = GraphStore::in_memory().unwrap();
        for i in 0..5 {
            let sym = nestweaver_schema::Symbol {
                uid: format!("sym:test:abc:{i}"),
                name: if i == 0 {
                    "analyzeBlastRadius".to_string()
                } else {
                    format!("helper{i}")
                },
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid: "repo:test".to_string(),
                file_path: "src/main.js".to_string(),
                start_line: 10 + i as u32,
                end_line: 10 + i as u32,
                signature: format!("function s{i}()"),
                summary: None,
                content_hash: format!("h{i}"),
                embedding: None,
                pagerank_score: Some(0.5),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: nestweaver_schema::Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            };
            store.insert_symbol(&sym).unwrap();
        }

        // Targeted: only the one matching symbol, cap irrelevant, not capped.
        let hit = generate_symbol_summaries_bounded(&store, Some("blastradius"), 1).unwrap();
        assert_eq!(hit.summaries.len(), 1);
        assert_eq!(hit.matched_total, 1);
        assert!(!hit.capped);
        assert_eq!(hit.summaries[0].target_name, "analyzeBlastRadius");

        // Untargeted with cap=2 over 5 symbols: bounded to 2 and flagged capped.
        let all = generate_symbol_summaries_bounded(&store, None, 2).unwrap();
        assert_eq!(all.summaries.len(), 2);
        assert_eq!(all.matched_total, 5);
        assert!(all.capped);
    }

    #[test]
    fn symbol_summaries_pushdown_does_not_substring_match_uid() {
        // Same regression as filter_by_target: "abc" appears in every test UID
        // (`sym:test:abc:{i}`) but in no name/file path — a UID substring match
        // would retain every symbol and defeat the pushdown.
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&nestweaver_schema::Symbol {
                uid: "sym:test:abc:1".to_string(),
                name: "foo".to_string(),
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid: "repo:test".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn foo()".to_string(),
                summary: None,
                content_hash: "a".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: nestweaver_schema::Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();

        let miss = generate_symbol_summaries_bounded(&store, Some("abc"), 10).unwrap();
        assert_eq!(
            miss.matched_total, 0,
            "a UID-hash substring must not match; matched: {}",
            miss.matched_total
        );

        // Exact UID, name substring, and file-path substring all still match.
        let by_uid = generate_symbol_summaries_bounded(&store, Some("sym:test:abc:1"), 10).unwrap();
        assert_eq!(by_uid.matched_total, 1, "exact UID must match");
        let by_path = generate_symbol_summaries_bounded(&store, Some("lib.rs"), 10).unwrap();
        assert_eq!(by_path.matched_total, 1, "file-path substring must match");
    }

    #[test]
    fn file_summaries_from_store() {
        let store = GraphStore::in_memory().unwrap();
        let sym1 = nestweaver_schema::Symbol {
            uid: "sym:test:abc:1".to_string(),
            name: "foo".to_string(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo:test".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: "fn foo()".to_string(),
            summary: None,
            content_hash: "a".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        let sym2 = nestweaver_schema::Symbol {
            uid: "sym:test:abc:20".to_string(),
            name: "bar".to_string(),
            kind: nestweaver_schema::SymbolKind::Class,
            repo_uid: "repo:test".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 20,
            end_line: 20,
            signature: "class Bar".to_string(),
            summary: None,
            content_hash: "b".to_string(),
            embedding: None,
            pagerank_score: Some(0.3),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        store.insert_symbol(&sym1).unwrap();
        store.insert_symbol(&sym2).unwrap();

        let summaries = generate_file_summaries(&store).unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].content.contains("src/lib.rs"));
        assert!(summaries[0].content.contains("exports 2 symbols"));
        assert!(summaries[0].content.contains("foo (Function)"));
        assert!(summaries[0].content.contains("bar (Class)"));
    }

    #[test]
    fn sidecar_path_appends_suffix() {
        let db = Path::new("/tmp/test.lbug");
        let expected = PathBuf::from("/tmp/test.lbug.summaries.json");
        assert_eq!(sidecar_path(db), expected);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");

        let summaries = vec![Summary {
            level: SummaryLevel::File,
            target_uid: "src/main.rs".to_string(),
            target_name: "src/main.rs".to_string(),
            content: "src/main.rs: exports 1 symbols: main (Function) | imports from: []"
                .to_string(),
            token_estimate: 17,
            file_path: None,
        }];

        save_summaries(&db_path, 1, &summaries, &Default::default()).unwrap();
        let loaded = load_summaries(&db_path, 1).unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].target_uid, "src/main.rs");
        assert_eq!(loaded[0].content, summaries[0].content);
    }

    #[test]
    fn stale_generation_is_treated_as_miss() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let summaries = vec![Summary {
            level: SummaryLevel::File,
            target_uid: "f".to_string(),
            target_name: "f".to_string(),
            content: "c".to_string(),
            token_estimate: 1,
            file_path: None,
        }];

        save_summaries(&db_path, 1, &summaries, &Default::default()).unwrap();
        // Same generation → hit.
        assert!(load_summaries(&db_path, 1).unwrap().is_some());
        // A newer generation (after a reindex) must be a MISS, never a
        // stale-served hit — the caller regenerates.
        assert!(
            load_summaries(&db_path, 2).unwrap().is_none(),
            "summaries from an older generation must not be served after a reindex"
        );
    }

    #[test]
    fn hub_role_classifies_by_degree_balance() {
        assert_eq!(hub_role(0, 0), "isolated");
        assert_eq!(hub_role(30, 2), "sink/utility"); // heavily called, calls little
        assert_eq!(hub_role(2, 30), "orchestrator"); // calls widely, called little
        assert_eq!(hub_role(10, 10), "bridge"); // balanced high degree
    }

    #[test]
    fn load_summaries_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nonexistent.lbug");
        let result = load_summaries(&db_path, 0).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn sidecar_path_uses_push_convention() {
        use std::path::Path;
        // The sidecar path should append ".summaries.json" to the full db
        // path (push), NOT replace the extension (with_extension).
        let p = sidecar_path(Path::new("data.lbug"));
        assert_eq!(p, Path::new("data.lbug.summaries.json"));

        // Ensure multi-dot paths also work correctly.
        let p2 = sidecar_path(Path::new("my.project.lbug"));
        assert_eq!(p2, Path::new("my.project.lbug.summaries.json"));
    }

    #[test]
    fn save_load_round_trip_preserves_levels() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");

        let file_summaries = vec![Summary {
            level: SummaryLevel::File,
            target_uid: "src/main.rs".to_string(),
            target_name: "src/main.rs".to_string(),
            content: "file summary".to_string(),
            token_estimate: 5,
            file_path: None,
        }];
        let symbol_summaries = [Summary {
            level: SummaryLevel::Symbol,
            target_uid: "sym:main".to_string(),
            target_name: "main".to_string(),
            content: "symbol summary".to_string(),
            token_estimate: 5,
            file_path: None,
        }];

        // Save file-level summaries.
        save_summaries(&db_path, 1, &file_summaries, &Default::default()).unwrap();

        // Merge with symbol-level summaries (as the MCP tool does).
        let mut all = load_summaries(&db_path, 1)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter(|s| s.level != SummaryLevel::Symbol)
            .collect::<Vec<_>>();
        all.extend(symbol_summaries.iter().cloned());
        save_summaries(&db_path, 1, &all, &Default::default()).unwrap();

        // Load back and verify both levels are present.
        let loaded = load_summaries(&db_path, 1).unwrap().unwrap();
        let file_count = loaded
            .iter()
            .filter(|s| s.level == SummaryLevel::File)
            .count();
        let sym_count = loaded
            .iter()
            .filter(|s| s.level == SummaryLevel::Symbol)
            .count();
        assert_eq!(file_count, 1, "file summaries should be preserved");
        assert_eq!(sym_count, 1, "symbol summaries should be saved");
    }
}

#[cfg(test)]
mod cap_disclosure_tests {
    use super::*;

    /// nw-270: `capped` and `matched_total` exist so callers can disclose the
    /// cap, and the `summary --json` caller discarded both — reporting
    /// `total: 500, truncated: false` for a 500-of-N answer. The cap was
    /// invisible in precisely the field that exists to reveal it.
    ///
    /// Pinned here, at the producer, because the guarantee callers depend on is
    /// that these two fields describe the PRE-cap world.
    #[test]
    fn capped_results_report_the_pre_cap_total() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();

        for i in 0..12 {
            store
                .insert_symbol(&nestweaver_schema::Symbol {
                    uid: format!("sym:test:s{i}"),
                    name: format!("s{i}"),
                    kind: nestweaver_schema::SymbolKind::Function,
                    repo_uid: "repo:test:demo".to_string(),
                    file_path: "a.js".to_string(),
                    start_line: 1,
                    end_line: 1,
                    signature: format!("fn s{i}()"),
                    summary: None,
                    content_hash: "hash".to_string(),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: nestweaver_schema::Visibility::Inferred,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
        }

        let capped = generate_symbol_summaries_bounded(&store, None, 5).unwrap();
        assert_eq!(capped.summaries.len(), 5, "the cap must actually bind");
        assert!(capped.capped, "a bound cap must be disclosed as capped");
        assert!(
            capped.matched_total > capped.summaries.len(),
            "matched_total must describe the PRE-cap world ({} matched vs {} returned)",
            capped.matched_total,
            capped.summaries.len()
        );

        // The counterweight: a cap that does NOT bind must not claim it did.
        // Without this, always-true `capped` would satisfy the assertion above.
        let uncapped = generate_symbol_summaries_bounded(&store, None, 1000).unwrap();
        assert!(
            !uncapped.capped,
            "a cap larger than the corpus must not report truncation"
        );
        assert_eq!(
            uncapped.matched_total,
            uncapped.summaries.len(),
            "and its total must equal what it returned"
        );
    }
}

#[cfg(test)]
mod cap_provenance_tests {
    use super::*;

    fn hub(uid: &str) -> Summary {
        Summary {
            level: SummaryLevel::Hub,
            target_uid: uid.to_string(),
            target_name: uid.to_string(),
            content: "x".to_string(),
            token_estimate: 1,
            file_path: None,
        }
    }

    /// nw-361. The cap has to survive the round trip, or a reader of the
    /// sidecar has no way to distinguish a capped set from a complete one —
    /// which is the whole defect, one layer down from `get_summary`.
    #[test]
    fn the_generators_drop_count_survives_the_sidecar_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        save_summaries(
            &db_path,
            7,
            &[hub("sym:a")],
            &cap_provenance(SummaryLevel::Hub, 10),
        )
        .unwrap();

        let (rows, drops) = load_summaries_with_cap(&db_path, 7).unwrap().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            drops.get("hub").copied(),
            Some(10),
            "the count the generator recorded must come back: {drops:?}"
        );
    }

    /// COUNTERWEIGHT, and the one real decision in this item: a sidecar with
    /// NO recorded provenance must not read as "nothing was dropped". It is
    /// reported as a MISS so the caller regenerates and records, rather than
    /// republishing a completeness claim nobody made.
    ///
    /// The plain `load_summaries` sibling still serves it, because callers
    /// that claim nothing lose nothing by reading it.
    #[test]
    fn a_sidecar_that_recorded_no_cap_is_a_miss_not_a_claim_of_completeness() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        // Hand-write the pre-nw-361 shape: summaries and a generation, no
        // `cap_dropped` key at all.
        std::fs::write(
            sidecar_path(&db_path),
            serde_json::json!({
                "summaries": [ {
                    "level": "Hub", "target_uid": "sym:a", "target_name": "a",
                    "content": "x", "token_estimate": 1
                } ],
                "graph_generation": 7
            })
            .to_string(),
        )
        .unwrap();

        assert!(
            load_summaries_with_cap(&db_path, 7).unwrap().is_none(),
            "an unrecorded cap must not be served to a caller that will publish \
             `truncated_by_cap`"
        );
        assert!(
            load_summaries(&db_path, 7).unwrap().is_some(),
            "the plain sibling claims nothing and must keep serving it, or this \
             change becomes a silent cache invalidation for every reader"
        );
    }

    /// An EMPTY map is a positive statement — "this writer looked and the cap
    /// did not fire" — and must be served, or a level with no cap would
    /// regenerate on every call.
    #[test]
    fn a_recorded_zero_is_a_statement_and_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        save_summaries(
            &db_path,
            7,
            &[hub("sym:a")],
            &cap_provenance(SummaryLevel::Hub, 0),
        )
        .unwrap();
        let (_, drops) = load_summaries_with_cap(&db_path, 7).unwrap().unwrap();
        assert_eq!(drops.get("hub").copied(), Some(0));
    }

    /// `merge_and_save_summaries` owns the "keep other levels, replace this
    /// level" invariant, so the per-level counts have to merge the same way —
    /// warming one level must not erase another level's recorded cap.
    #[test]
    fn warming_one_level_keeps_another_levels_recorded_cap() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let cluster = Summary {
            level: SummaryLevel::Cluster,
            target_uid: "cluster:1".to_string(),
            target_name: "c".to_string(),
            content: "x".to_string(),
            token_estimate: 1,
            file_path: None,
        };
        save_summaries(
            &db_path,
            7,
            &[cluster],
            &cap_provenance(SummaryLevel::Cluster, 21),
        )
        .unwrap();

        merge_and_save_summaries(&db_path, 7, SummaryLevel::Hub, &[hub("sym:a")], 10);

        let (rows, drops) = load_summaries_with_cap(&db_path, 7).unwrap().unwrap();
        assert_eq!(rows.len(), 2, "both levels must survive: {rows:?}");
        assert_eq!(
            drops.get("cluster").copied(),
            Some(21),
            "warming `hub` erased what `cluster` recorded: {drops:?}"
        );
        assert_eq!(drops.get("hub").copied(), Some(10), "{drops:?}");
    }
}
