use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::{GraphStore, SymbolBasic};

use crate::clustering::{self, Graph};

/// Top-level clustering output, persisted to the sidecar JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusteringOutput {
    pub resolution: f64,
    pub modularity: f64,
    pub communities: Vec<CommunityInfo>,
}

/// A single detected community (cluster) of code symbols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityInfo {
    pub id: u32,
    pub name: String,
    pub cohesion: f64,
    pub member_count: usize,
    pub members: Vec<ClusterMember>,
    pub key_files: Vec<String>,
}

/// A symbol that belongs to a community.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMember {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub kind: String,
}

/// Symbol count above which the adaptive default drops to
/// [`LARGE_GRAPH_CLUSTER_RESOLUTION`].
pub const LARGE_GRAPH_SYMBOL_THRESHOLD: usize = 10_000;

/// Resolution used on graphs above [`LARGE_GRAPH_SYMBOL_THRESHOLD`]. Lower
/// resolution merges communities more aggressively, avoiding the explosion of
/// near-singleton communities that a high resolution produces at scale.
pub const LARGE_GRAPH_CLUSTER_RESOLUTION: f64 = 0.3;

/// Resolution used on graphs at or below [`LARGE_GRAPH_SYMBOL_THRESHOLD`].
pub const SMALL_GRAPH_CLUSTER_RESOLUTION: f64 = 0.5;

/// The resolution to use when the caller named none.
///
/// F-DC-7 (folded into nw-299). Community IDs are assignment-dependent, so two
/// runs at different resolutions produce two different ID SPACES. The `clusters`
/// tool and the `clusters`/`cluster` CLI commands each open-coded this same
/// 0.3/0.5 rule, while `generate_cluster_summaries` hard-coded **1.0** — so
/// `summary --level cluster` emitted IDs from a partition that `cluster <id>`
/// could not resolve, and 26 of 50 IDs came back wrong. That is what two
/// independent partitions of one graph look like.
///
/// Every default now comes from here, so the ID spaces cannot diverge again.
/// A caller that passes an explicit resolution still gets exactly that.
pub fn default_cluster_resolution(store: &GraphStore) -> f64 {
    default_cluster_resolution_for_symbol_count(store.count_symbols().unwrap_or(0))
}

/// The size-keyed half of [`default_cluster_resolution`], taking the symbol
/// count directly rather than a store.
///
/// Task 4.7b (nw-479): `clusters --repo` computes on the repo-INDUCED
/// SUBGRAPH, and per the owner decision (plan §4 Q4) the adaptive default
/// must be keyed on the *subgraph's* symbol count, not the whole graph's —
/// otherwise a small repo scoped out of a large multi-repo graph would still
/// get the low, aggressive-merge resolution meant for >10K-symbol graphs,
/// even though its own induced subgraph is tiny (Fortunato & Barthélemy,
/// PNAS 104:36, 2007 — modularity's resolution limit). Splitting this out
/// lets both the unscoped and scoped paths share one rule instead of growing
/// a second copy of the threshold.
pub fn default_cluster_resolution_for_symbol_count(symbol_count: usize) -> f64 {
    if symbol_count > LARGE_GRAPH_SYMBOL_THRESHOLD {
        LARGE_GRAPH_CLUSTER_RESOLUTION
    } else {
        SMALL_GRAPH_CLUSTER_RESOLUTION
    }
}

/// Clamp a caller-supplied resolution to a finite, positive value before it
/// is used in a computation or persisted to a sidecar.
///
/// Shared by [`compute_clusters`] and [`compute_clusters_scoped`]: `leiden`
/// clamps invalid resolutions for the computation, but the raw value is also
/// persisted/returned, and NaN/inf serialize as `null`, which then fails to
/// parse back (f64 != null).
fn sanitize_resolution(resolution: f64) -> f64 {
    if resolution.is_finite() && resolution > 0.0 {
        resolution
    } else {
        1.0
    }
}

/// Run Louvain-style local-moving community detection on the code graph.
///
/// Loads all Symbol nodes and code-level edges (CALLS, IMPORTS, EXTENDS_SYM,
/// IMPLEMENTS_SYM, MEMBER_OF) from the store, builds an undirected weighted
/// graph, runs the Louvain-style local-moving algorithm (single-level; no
/// Leiden refinement/aggregation), and returns structured output.
pub fn compute_clusters(store: &GraphStore, resolution: f64) -> Result<ClusteringOutput> {
    // Sanitize BEFORE the value is stored in `ClusteringOutput`: `leiden`
    // clamps invalid resolutions for the computation, but the raw value is
    // also persisted to the sidecar JSON — and NaN/inf serialize as `null`,
    // which then fails to parse in `load_clusters` (f64 != null).
    let resolution = sanitize_resolution(resolution);
    let (symbols, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!(e))
        .context("failed to load graph data for clustering")?;

    if symbols.is_empty() {
        return Ok(ClusteringOutput {
            resolution,
            modularity: 0.0,
            communities: vec![],
        });
    }

    // Build UID -> index mapping.
    let uid_to_idx: HashMap<&str, usize> = symbols
        .iter()
        .enumerate()
        .map(|(i, s)| (s.uid.as_str(), i))
        .collect();

    let n = symbols.len();

    // Build adjacency list (undirected, confidence as weight).
    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![vec![]; n];
    let mut total_weight = 0.0;

    for (src, dst, confidence) in &edges {
        if let (Some(&si), Some(&di)) = (uid_to_idx.get(src.as_str()), uid_to_idx.get(dst.as_str()))
        {
            neighbors[si].push((di, *confidence));
            neighbors[di].push((si, *confidence));
            total_weight += *confidence;
        }
    }

    let graph = Graph {
        n,
        neighbors,
        total_weight,
    };

    // Run Louvain-style local-moving clustering.
    let result = clustering::leiden(&graph, resolution, 100);

    // Build community output from the result. Local member index == index
    // into `symbols` here, since the graph was built over the WHOLE corpus.
    let communities = build_community_infos(&symbols, |local_idx| local_idx, &result.communities);

    Ok(ClusteringOutput {
        resolution,
        modularity: result.modularity,
        communities,
    })
}

/// The scope a [`compute_clusters_scoped`] run was computed under.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterScope {
    /// The resolved repo UIDs the induced subgraph was built from, sorted
    /// for a deterministic, diff-friendly response.
    pub repos: Vec<String>,
    /// Count of edges with EXACTLY one endpoint inside `repos` — i.e. edges
    /// that touch the scope but were cut because the graph is induced, not
    /// filtered. An edge with BOTH endpoints outside `repos` is unrelated to
    /// this scope and is not counted; an edge with both endpoints inside is
    /// part of the subgraph and is not "excluded".
    pub cross_repo_edges_excluded: usize,
    /// Always `"repo_scoped"`. Community `id`s in a scoped run are assigned
    /// by a Louvain run over a DIFFERENT graph than the global one `clusters`
    /// (no `repos`) computes and persists to `<db>.clusters.json` — so a
    /// scoped id and a global id at the same integer value name two
    /// unrelated communities. This tag lets a caller (and, at the CLI/MCP
    /// layer, `cluster <id>`) refuse to resolve a scoped id against the
    /// global sidecar rather than silently returning the wrong community.
    pub id_space: String,
}

/// [`ClusteringOutput`] plus the [`ClusterScope`] it was computed under.
///
/// Deliberately a SEPARATE type from `ClusteringOutput` rather than an
/// `Option<ClusterScope>` field bolted onto it: `ClusteringOutput` is the
/// sidecar's on-disk shape (`save_clusters`/`load_clusters`), and a scoped
/// result must never reach that sidecar (see [`compute_clusters_scoped`]).
/// Keeping the types distinct makes "this is the scoped path" visible at
/// every call site instead of relying on callers to check a flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopedClusteringOutput {
    pub resolution: f64,
    pub modularity: f64,
    pub communities: Vec<CommunityInfo>,
    pub scope: ClusterScope,
}

/// Run Louvain-style local-moving community detection on the subgraph
/// INDUCED by `repo_selectors` — symbols whose `repo_uid` resolves into the
/// selector set, and only the edges with BOTH endpoints among them.
///
/// Task 4.7b (nw-479), owner decision (plan §4 Q4): a caller asking for one
/// repo's clusters expects numbers ABOUT that repo. Filtering the GLOBAL
/// partition's communities down to their in-scope members (the `hubs`/
/// `bridges` precedent, nw-468) would leave `cohesion`/`key_files` describing
/// neighbours in repos the caller cannot see, and a big cross-repo community
/// could swallow a repo's whole architecture into one opaque bucket. Running
/// Louvain on the induced subgraph instead gives every returned community a
/// cohesion and membership computed ENTIRELY from the scope the caller asked
/// about.
///
/// `resolution` follows an explicit value when given; otherwise the default
/// is [`default_cluster_resolution_for_symbol_count`] applied to the
/// SUBGRAPH's symbol count, not the whole graph's — the adaptive threshold
/// exists to avoid the near-singleton-community explosion of a high
/// resolution at scale, and "at scale" has to mean the graph actually being
/// partitioned.
///
/// `repo_selectors` is resolved with [`crate::node_scope::resolve_repo_filter`]
/// — the same resolver `--repo`/`repos` use everywhere else — so an unknown
/// or ambiguous selector ERRORS rather than silently producing an empty
/// scope. `repo_selectors` must be non-empty: an empty scope is a contradiction
/// for a function whose whole contract is "induced by these repos" (the
/// unscoped [`compute_clusters`] is the right call for "no repo filter").
///
/// This function NEVER writes `<db>.clusters.json` or any resolution-keyed
/// sidecar — callers must not pass its output to [`save_clusters`]. Doing so
/// would let a scoped, partial-graph partition silently become the
/// last-writer-wins answer that `hub_nodes`/`bridge_nodes`/`blast_radius`/
/// `cluster <id>` (with no `--resolution`) read for the WHOLE graph.
pub fn compute_clusters_scoped(
    store: &GraphStore,
    repo_selectors: &[String],
    resolution: Option<f64>,
) -> Result<ScopedClusteringOutput> {
    if repo_selectors.is_empty() {
        return Err(anyhow::anyhow!(
            "compute_clusters_scoped requires at least one repo selector; \
             call compute_clusters for an unscoped run"
        ));
    }

    let resolved_repos = crate::node_scope::resolve_repo_filter(store, repo_selectors, None)
        .context("resolving clusters --repo scope")?;

    let (symbols, edges) = store
        .load_code_symbols_and_edges()
        .map_err(|e| anyhow::anyhow!(e))
        .context("failed to load graph data for scoped clustering")?;

    let mut scope_repos: Vec<String> = resolved_repos.iter().cloned().collect();
    scope_repos.sort();

    if symbols.is_empty() {
        let resolution = sanitize_resolution(
            resolution.unwrap_or_else(|| default_cluster_resolution_for_symbol_count(0)),
        );
        return Ok(ScopedClusteringOutput {
            resolution,
            modularity: 0.0,
            communities: vec![],
            scope: ClusterScope {
                repos: scope_repos,
                cross_repo_edges_excluded: 0,
                id_space: "repo_scoped".to_string(),
            },
        });
    }

    // Global uid -> full-symbol index, needed to classify EVERY edge
    // endpoint (including ones outside scope) so a cross-repo exclusion can
    // be counted even though it never enters the induced subgraph below.
    let global_uid_to_idx: HashMap<&str, usize> = symbols
        .iter()
        .enumerate()
        .map(|(i, s)| (s.uid.as_str(), i))
        .collect();
    let in_scope = |global_idx: usize| resolved_repos.contains(&symbols[global_idx].repo_uid);

    // The induced subgraph's node set: only in-scope symbols, reindexed to a
    // dense local `0..n` range so `Graph`/`leiden` never allocate or iterate
    // over out-of-scope nodes.
    let scoped_indices: Vec<usize> = (0..symbols.len()).filter(|&i| in_scope(i)).collect();
    let global_to_local: HashMap<usize, usize> = scoped_indices
        .iter()
        .enumerate()
        .map(|(local, &global)| (global, local))
        .collect();

    let n = scoped_indices.len();
    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![vec![]; n];
    let mut total_weight = 0.0;
    let mut cross_repo_edges_excluded: usize = 0;

    for (src, dst, confidence) in &edges {
        let (Some(&sg), Some(&dg)) = (
            global_uid_to_idx.get(src.as_str()),
            global_uid_to_idx.get(dst.as_str()),
        ) else {
            continue;
        };
        match (in_scope(sg), in_scope(dg)) {
            // `.get()`, not indexing: `sg`/`dg` are in scope by construction
            // here (every in-scope global index was inserted into
            // `global_to_local` above), so this should always hit -- but a
            // future edit to either construction site must not be able to
            // turn that invariant into a panic. A miss is treated the same
            // as "not part of the induced subgraph" rather than crashing.
            (true, true) => {
                if let (Some(&sl), Some(&dl)) = (global_to_local.get(&sg), global_to_local.get(&dg))
                {
                    neighbors[sl].push((dl, *confidence));
                    neighbors[dl].push((sl, *confidence));
                    total_weight += *confidence;
                }
            }
            // Exactly one endpoint is in scope: this edge touches the scope
            // but is cut by the induced-subgraph boundary.
            (true, false) | (false, true) => cross_repo_edges_excluded += 1,
            // Neither endpoint is in scope: unrelated to this scope entirely.
            (false, false) => {}
        }
    }

    let graph = Graph {
        n,
        neighbors,
        total_weight,
    };

    let resolution = sanitize_resolution(
        resolution.unwrap_or_else(|| default_cluster_resolution_for_symbol_count(n)),
    );

    let result = clustering::leiden(&graph, resolution, 100);

    // Local member index -> index into the FULL `symbols` slice, via the
    // subgraph's own node list.
    let communities = build_community_infos(
        &symbols,
        |local_idx| scoped_indices[local_idx],
        &result.communities,
    );

    Ok(ScopedClusteringOutput {
        resolution,
        modularity: result.modularity,
        communities,
        scope: ClusterScope {
            repos: scope_repos,
            cross_repo_edges_excluded,
            id_space: "repo_scoped".to_string(),
        },
    })
}

/// Build [`CommunityInfo`] rows from a [`clustering::ClusteringResult`],
/// shared by [`compute_clusters`] (identity mapping — the graph was built
/// over the whole `symbols` slice) and [`compute_clusters_scoped`] (maps a
/// local subgraph member index back to its index in the full `symbols`
/// slice via `scoped_indices`).
fn build_community_infos(
    symbols: &[SymbolBasic],
    local_to_symbol_idx: impl Fn(usize) -> usize,
    communities: &[clustering::Community],
) -> Vec<CommunityInfo> {
    let mut out: Vec<CommunityInfo> = Vec::new();
    for community in communities {
        let members: Vec<ClusterMember> = community
            .members
            .iter()
            .map(|&local_idx| {
                let s = &symbols[local_to_symbol_idx(local_idx)];
                ClusterMember {
                    uid: s.uid.clone(),
                    name: s.name.clone(),
                    file_path: s.file_path.clone(),
                    kind: s.kind.clone(),
                }
            })
            .collect();

        let name = derive_cluster_name(&members);

        // Key files: unique file paths sorted by frequency descending (top 5).
        let mut file_counts: HashMap<&str, usize> = HashMap::new();
        for m in &members {
            *file_counts.entry(m.file_path.as_str()).or_default() += 1;
        }
        let mut file_pairs: Vec<(String, usize)> = file_counts
            .into_iter()
            .map(|(f, c)| (f.to_string(), c))
            .collect();
        // Count descending, then file path ascending as a deterministic
        // tie-break — otherwise equal-count files keep the source HashMap's
        // per-process iteration order, making key_files (and thus the clusters
        // output) drift between runs (nw-088).
        file_pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let key_files: Vec<String> = file_pairs.into_iter().take(5).map(|(f, _)| f).collect();

        out.push(CommunityInfo {
            id: community.id,
            name,
            cohesion: community.cohesion,
            member_count: members.len(),
            members,
            key_files,
        });
    }

    // Sort communities by size descending.
    out.sort_by_key(|c| std::cmp::Reverse(c.member_count));
    out
}

/// Derive a human-readable name for a cluster from its members.
///
/// Strategy: find the longest common directory prefix among member file paths.
/// Falls back to the first member's name if no common prefix exists.
fn derive_cluster_name(members: &[ClusterMember]) -> String {
    if members.is_empty() {
        return "unnamed".to_string();
    }
    let paths: Vec<&str> = members.iter().map(|m| m.file_path.as_str()).collect();
    if let Some(common) = common_path_prefix(&paths)
        && !common.is_empty()
    {
        return common;
    }
    members[0].name.clone()
}

/// Find the longest common directory prefix of a set of file paths.
///
/// Returns `None` if paths are empty, there is only one path, or no common
/// directory segments exist across multiple paths.
fn common_path_prefix(paths: &[&str]) -> Option<String> {
    if paths.len() < 2 {
        return None;
    }
    let parts: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| p.split('/').collect::<Vec<_>>())
        .collect();
    let min_len = parts.iter().map(|p| p.len()).min().unwrap_or(0);
    if min_len <= 1 {
        return None;
    }

    let mut prefix_len = 0;
    for i in 0..min_len.saturating_sub(1) {
        // exclude filename
        if parts.iter().all(|p| p[i] == parts[0][i]) {
            prefix_len = i + 1;
        } else {
            break;
        }
    }
    if prefix_len == 0 {
        return None;
    }
    Some(parts[0][..prefix_len].join("/"))
}

/// Compute the sidecar file path for cluster data: `<db>.clusters.json`.
///
/// nw-401: this is the "last-writer-wins" canonical path — every
/// `save_clusters` call overwrites it regardless of resolution, and every
/// caller that does not care WHICH resolution answered (the `hubs`/`bridges`
/// cluster-attachment paths, `blast_radius`, and `cluster <id>` with no
/// `--resolution`) reads it. It is kept, unchanged, for exactly those
/// callers. What changed is that it is no longer the ONLY record: see
/// [`sidecar_path_for_resolution`].
pub fn sidecar_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".clusters.json")
}

/// The sidecar file path for cluster data at a SPECIFIC resolution:
/// `<db>.clusters.<resolution>.json`.
///
/// nw-401. The single unkeyed sidecar meant `clusters --resolution 0.5`
/// followed by an unrelated `clusters --resolution 5.0` silently reinterpreted
/// every subsequent `cluster <id>` call: ~24 of 27 community IDs remap between
/// resolutions on a real graph, so this was not an edge case. Keying by
/// resolution lets multiple resolutions' clusterings coexist on disk so an
/// explicit `cluster --resolution R` can find R's own data even after a later
/// run at a different resolution overwrote the canonical file.
///
/// Formatted with `{:e}` (exponential notation) rather than `{}` or a fixed
/// number of decimal places: Rust's float formatting is the shortest
/// round-trippable representation in either mode, so two DIFFERENT
/// resolutions never collide on the same filename the way fixed-precision
/// truncation would (`0.0000001` and `0.0000005` both round to `0.000000` at
/// 6 decimals; `1e-7` and `5e-7` do not collide as exponential strings).
pub fn sidecar_path_for_resolution(db_path: &Path, resolution: f64) -> PathBuf {
    crate::sidecar_path(db_path, &format!(".clusters.{resolution:e}.json"))
}

/// On-disk shape of a clusters sidecar file: the [`ClusteringOutput`] plus the
/// [`GraphStore::graph_generation`](nestweaver_store::GraphStore::graph_generation)
/// the clustering was computed from.
///
/// nw-646: the sidecar previously carried no generation marker at all, so a
/// days-old cache (built before a reindex changed the graph) was served
/// forever as long as `--resolution` matched — `clusters --json` reported
/// "Using cached clusters" with a modularity that no longer matched the live
/// graph. `#[serde(flatten)]` keeps the on-disk JSON's `resolution` /
/// `modularity` / `communities` keys exactly as they were (an older sidecar
/// written before this change still parses), while `graph_generation`
/// defaults to `None` on a read of that older shape — an ABSENT generation
/// is treated as unknown/stale by every caller that cares, rather than as a
/// parse error.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClusterSidecarFile {
    #[serde(flatten)]
    output: ClusteringOutput,
    #[serde(default)]
    graph_generation: Option<u64>,
}

/// Atomically write `output` (plus its `graph_generation`) as JSON to `path`.
///
/// Writes to a process-unique temp file and renames into place, so a
/// concurrent reader (e.g. `hub_nodes` racing a `clusters` call) never
/// observes a partially-written file.
fn write_clusters_atomic(
    path: &Path,
    output: &ClusteringOutput,
    graph_generation: Option<u64>,
) -> Result<()> {
    let file = ClusterSidecarFile {
        output: output.clone(),
        graph_generation,
    };
    let json =
        serde_json::to_string_pretty(&file).context("failed to serialize clustering output")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, json).with_context(|| format!("failed to write {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to move {} into place", path.display()));
    }
    Ok(())
}

/// Persist clustering output to the sidecar file(s), tagged with the graph
/// generation it was computed from.
///
/// nw-401: writes BOTH the canonical last-writer-wins path (unchanged
/// behavior, for callers that want "whatever was computed most recently") AND
/// a resolution-keyed copy (so a caller that later pins `--resolution` can
/// still find THIS run's data, undisturbed by a later run at a different
/// resolution). Concurrent writers at the SAME resolution still resolve to
/// last-writer-wins on the keyed path too — acceptable, because the output is
/// deterministic for a given graph state and resolution.
///
/// nw-646: `graph_generation` is `store.graph_generation()` at the time the
/// output was computed. A caller that has no meaningful generation to record
/// (e.g. a scoped or in-memory computation not persisted for staleness
/// tracking) may still pass one — the value is opaque to this function, which
/// only stores what it is given.
pub fn save_clusters(
    db_path: &Path,
    output: &ClusteringOutput,
    graph_generation: u64,
) -> Result<()> {
    write_clusters_atomic(&sidecar_path(db_path), output, Some(graph_generation))?;
    write_clusters_atomic(
        &sidecar_path_for_resolution(db_path, output.resolution),
        output,
        Some(graph_generation),
    )?;
    Ok(())
}

/// Load a clustering sidecar file (output + generation marker) from a
/// specific path.
fn load_clusters_from(path: &Path) -> Result<Option<ClusterSidecarFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let json =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let file: ClusterSidecarFile =
        serde_json::from_str(&json).context("failed to parse clusters sidecar")?;
    Ok(Some(file))
}

/// Load clustering output from the canonical (unkeyed) sidecar file, if it
/// exists.
///
/// Returns `Ok(None)` when the sidecar does not exist (i.e. clusters have
/// never been computed for this database). This is "whatever was computed
/// most recently, at whichever resolution" — the same last-writer-wins
/// semantics this function has always had, and it does NOT check the graph
/// generation — callers that care whether the cache is stale relative to the
/// current graph must use [`load_clusters_with_generation`] instead. Callers
/// that need a SPECIFIC resolution, immune to a later differently-resolved
/// run, must use [`load_clusters_for_resolution`] instead.
pub fn load_clusters(db_path: &Path) -> Result<Option<ClusteringOutput>> {
    Ok(load_clusters_from(&sidecar_path(db_path))?.map(|f| f.output))
}

/// Load clustering output from the canonical (unkeyed) sidecar file ALONGSIDE
/// the graph generation it was computed from, if it exists.
///
/// nw-646. The generation is `None` when the sidecar predates this field
/// (written by an older binary) — a caller must treat that the same as a
/// generation that does not match the current graph: unknown provenance is
/// not evidence of freshness.
pub fn load_clusters_with_generation(
    db_path: &Path,
) -> Result<Option<(ClusteringOutput, Option<u64>)>> {
    Ok(load_clusters_from(&sidecar_path(db_path))?.map(|f| (f.output, f.graph_generation)))
}

/// Load clustering output computed at EXACTLY `resolution`, if it has ever
/// been computed and saved for this database.
///
/// nw-401. Unlike [`load_clusters`], this cannot be poisoned by an unrelated
/// `clusters --resolution` run at a different resolution: it reads the
/// resolution-keyed sidecar, which a later run at a DIFFERENT resolution
/// never touches (it writes its own key). Returns `Ok(None)` when nobody has
/// computed clusters at this exact resolution for this database yet — the
/// caller must decide whether to compute it now or refuse.
pub fn load_clusters_for_resolution(
    db_path: &Path,
    resolution: f64,
) -> Result<Option<ClusteringOutput>> {
    Ok(load_clusters_from(&sidecar_path_for_resolution(db_path, resolution))?.map(|f| f.output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_path_prefix_basic() {
        let paths = &[
            "src/auth/login.rs",
            "src/auth/session.rs",
            "src/auth/middleware.rs",
        ];
        assert_eq!(common_path_prefix(paths), Some("src/auth".to_string()));
    }

    #[test]
    fn common_path_prefix_no_common() {
        let paths = &["src/a.rs", "lib/b.rs", "tests/c.rs"];
        assert_eq!(common_path_prefix(paths), None);
    }

    #[test]
    fn common_path_prefix_single_file() {
        let paths = &["src/main.rs"];
        // Only one segment before the filename — no meaningful directory prefix.
        assert_eq!(common_path_prefix(paths), None);
    }

    #[test]
    fn common_path_prefix_deep() {
        let paths = &["crates/engine/src/index.rs", "crates/engine/src/query.rs"];
        assert_eq!(
            common_path_prefix(paths),
            Some("crates/engine/src".to_string())
        );
    }

    #[test]
    fn derive_cluster_name_with_common_prefix() {
        let members = vec![
            ClusterMember {
                uid: "a".to_string(),
                name: "fn_a".to_string(),
                file_path: "src/auth/login.rs".to_string(),
                kind: "Function".to_string(),
            },
            ClusterMember {
                uid: "b".to_string(),
                name: "fn_b".to_string(),
                file_path: "src/auth/session.rs".to_string(),
                kind: "Function".to_string(),
            },
        ];
        assert_eq!(derive_cluster_name(&members), "src/auth");
    }

    #[test]
    fn derive_cluster_name_fallback() {
        let members = vec![ClusterMember {
            uid: "x".to_string(),
            name: "main".to_string(),
            file_path: "main.rs".to_string(),
            kind: "Function".to_string(),
        }];
        assert_eq!(derive_cluster_name(&members), "main");
    }

    #[test]
    fn sidecar_path_appends_extension() {
        let db = Path::new("/tmp/test.lbug");
        let expected = PathBuf::from("/tmp/test.lbug.clusters.json");
        assert_eq!(sidecar_path(db), expected);
    }

    /// nw-401. The defect: `clusters --resolution 0.5` then an unrelated
    /// `clusters --resolution 5.0` silently reinterpreted every later
    /// `cluster <id>` call, because both writes landed on the SAME unkeyed
    /// sidecar. This pins the fix: the resolution-keyed load must return
    /// EXACTLY what was saved at that resolution, unperturbed by a later save
    /// at a different resolution, while the canonical unkeyed load keeps its
    /// existing last-writer-wins behavior for callers that want it.
    #[test]
    fn a_later_resolution_cannot_poison_an_earlier_ones_keyed_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");

        let low = ClusteringOutput {
            resolution: 0.5,
            modularity: 0.42,
            communities: vec![CommunityInfo {
                id: 2,
                name: "low-res".to_string(),
                cohesion: 0.906,
                member_count: 36,
                members: vec![],
                key_files: vec![],
            }],
        };
        save_clusters(&db_path, &low, 1).unwrap();

        let high = ClusteringOutput {
            resolution: 5.0,
            modularity: 0.9,
            communities: vec![CommunityInfo {
                id: 2,
                name: "high-res".to_string(),
                cohesion: 0.266,
                member_count: 3,
                members: vec![],
                key_files: vec![],
            }],
        };
        save_clusters(&db_path, &high, 2).unwrap();

        // The keyed load for 0.5 must still see the FIRST run's data, even
        // though the second `save_clusters` ran after it and shares the
        // canonical unkeyed path.
        let pinned = load_clusters_for_resolution(&db_path, 0.5)
            .unwrap()
            .expect("resolution 0.5 was saved and must still be found");
        assert_eq!(pinned.communities[0].member_count, 36);
        assert_eq!(pinned.communities[0].name, "low-res");

        // The keyed load for 5.0 sees its own data too — this isn't a
        // one-survivor accident.
        let other = load_clusters_for_resolution(&db_path, 5.0)
            .unwrap()
            .expect("resolution 5.0 was saved and must be found");
        assert_eq!(other.communities[0].member_count, 3);

        // COUNTERWEIGHT: invert the claim. The UNKEYED canonical load is
        // documented as last-writer-wins and must still behave that way — the
        // fix must not have accidentally made EVERY load resolution-stable,
        // which would silently change behavior for callers (hubs/bridges
        // cluster-attachment, blast_radius) that rely on "whatever is most
        // recent".
        let canonical = load_clusters(&db_path).unwrap().unwrap();
        assert_eq!(
            canonical.communities[0].member_count, 3,
            "the canonical sidecar must still reflect the LAST save, not the first"
        );

        // A resolution nobody ever computed must not be silently satisfied by
        // partial-match filename luck.
        assert!(
            load_clusters_for_resolution(&db_path, 1.0)
                .unwrap()
                .is_none(),
            "a resolution that was never saved must not be found"
        );
    }

    #[test]
    fn compute_clusters_sanitizes_invalid_resolution_before_storing() {
        // Raw NaN/inf would serialize as `null` in the sidecar JSON and break
        // load_clusters; compute_clusters must store a finite positive value.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0] {
            let output = compute_clusters(&store, bad).unwrap();
            assert!(
                output.resolution.is_finite() && output.resolution > 0.0,
                "resolution {bad} must be sanitized before storing, got {}",
                output.resolution
            );
            save_clusters(&db_path, &output, store.graph_generation()).unwrap();
            let loaded = load_clusters(&db_path).unwrap().unwrap();
            assert_eq!(loaded.resolution, output.resolution);
        }
    }

    // ── nw-646: the clusters sidecar carries a graph-generation marker ──────

    /// `save_clusters` persists the generation it is given, and
    /// `load_clusters_with_generation` returns it back unchanged --
    /// `load_clusters` (the plain sibling) still returns just the output, so
    /// existing callers that never cared about staleness are unaffected.
    #[test]
    fn load_clusters_with_generation_round_trips_the_stored_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let output = ClusteringOutput {
            resolution: 0.5,
            modularity: 0.42,
            communities: vec![],
        };
        save_clusters(&db_path, &output, 331).unwrap();

        let (loaded, generation) = load_clusters_with_generation(&db_path).unwrap().unwrap();
        assert_eq!(generation, Some(331));
        assert_eq!(loaded.resolution, output.resolution);

        // COUNTERWEIGHT: the plain `load_clusters` sibling is unaffected --
        // still just the output, ignoring generation entirely.
        let plain = load_clusters(&db_path).unwrap().unwrap();
        assert_eq!(plain.resolution, output.resolution);
    }

    /// A sidecar written before this field existed (plain `ClusteringOutput`
    /// JSON, no `graph_generation` key) must parse as generation `None`
    /// rather than failing to deserialize -- an absent generation is
    /// "unknown provenance", which every staleness-checking caller must
    /// treat the same as "stale", not as a parse error.
    #[test]
    fn load_clusters_with_generation_treats_a_pre_nw_646_sidecar_as_generation_none() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let path = sidecar_path(&db_path);
        let legacy = ClusteringOutput {
            resolution: 0.5,
            modularity: 0.1,
            communities: vec![],
        };
        // Write the OLD shape directly -- no wrapper, no `graph_generation`
        // key -- rather than going through `save_clusters`, which always
        // writes the new shape.
        fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let (loaded, generation) = load_clusters_with_generation(&db_path).unwrap().unwrap();
        assert_eq!(
            generation, None,
            "a legacy sidecar has no generation marker"
        );
        assert_eq!(loaded.resolution, 0.5);

        // COUNTERWEIGHT: the plain loader must still read the legacy shape
        // fine -- this field being new must not break reading old sidecars
        // at all.
        let plain = load_clusters(&db_path).unwrap().unwrap();
        assert_eq!(plain.resolution, 0.5);
    }

    // ── Task 4.7b (nw-479): `clusters --repo` on the repo-induced subgraph ──

    mod repo_scope {
        use std::collections::HashSet;

        use nestweaver_schema::{EdgeType, Repo, ResolvedEdge, Symbol, SymbolKind, Visibility};

        use super::*;

        fn insert_test_repo(store: &GraphStore, uid: &str) {
            store
                .insert_repo(&Repo {
                    uid: uid.to_string(),
                    url: format!("https://example.test/{uid}"),
                    indexed_sha: String::new(),
                    staleness_commits_behind: 0,
                    instance_id: "default".to_string(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }

        fn make_symbol_in_repo(uid: &str, name: &str, repo_uid: &str) -> Symbol {
            Symbol {
                uid: uid.to_string(),
                name: name.to_string(),
                kind: SymbolKind::Function,
                repo_uid: repo_uid.to_string(),
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

        fn make_edge(src: &str, tgt: &str) -> ResolvedEdge {
            ResolvedEdge {
                source_uid: src.to_string(),
                target_uid: tgt.to_string(),
                edge_type: EdgeType::Calls,
                confidence: 1.0,
                link_type: None,
                evidence: vec![],
            }
        }

        /// nw-479 Task 4.7b. The global run merges two densely cross-linked
        /// repos into one community; a scoped run must instead cluster ONLY
        /// the induced subgraph -- repo-a's symbols and repo-a's internal
        /// edges -- never a repo-b symbol.
        #[test]
        fn clusters_repo_scope_runs_on_the_induced_subgraph() {
            let store = GraphStore::in_memory().unwrap();
            insert_test_repo(&store, "repo-a");
            insert_test_repo(&store, "repo-b");

            // repo-a: 4 fully-connected symbols.
            for i in 0..4 {
                store
                    .insert_symbol(&make_symbol_in_repo(
                        &format!("a{i}"),
                        &format!("a_fn_{i}"),
                        "repo-a",
                    ))
                    .unwrap();
            }
            for i in 0..4 {
                for j in 0..4 {
                    if i != j {
                        store
                            .insert_edge(&make_edge(&format!("a{i}"), &format!("a{j}")))
                            .unwrap();
                    }
                }
            }
            // repo-b: 4 fully-connected symbols.
            for i in 0..4 {
                store
                    .insert_symbol(&make_symbol_in_repo(
                        &format!("b{i}"),
                        &format!("b_fn_{i}"),
                        "repo-b",
                    ))
                    .unwrap();
            }
            for i in 0..4 {
                for j in 0..4 {
                    if i != j {
                        store
                            .insert_edge(&make_edge(&format!("b{i}"), &format!("b{j}")))
                            .unwrap();
                    }
                }
            }
            // Dense cross-links -- the FULL bipartite between the two
            // 4-cliques (16 pairs, both directions) -- so the cross-repo
            // weight dominates each clique's own 12 intra-repo directed
            // edges and the GLOBAL run merges both repos into one community.
            for i in 0..4 {
                for j in 0..4 {
                    store
                        .insert_edge(&make_edge(&format!("a{i}"), &format!("b{j}")))
                        .unwrap();
                    store
                        .insert_edge(&make_edge(&format!("b{j}"), &format!("a{i}")))
                        .unwrap();
                }
            }

            let resolution = 0.3;
            let global = compute_clusters(&store, resolution).unwrap();
            assert_eq!(
                global.communities.len(),
                1,
                "premise broken -- the global run must merge both repos into \
                 one community: {:?}",
                global
                    .communities
                    .iter()
                    .map(|c| c.member_count)
                    .collect::<Vec<_>>()
            );
            assert_eq!(global.communities[0].member_count, 8);

            let scoped =
                compute_clusters_scoped(&store, &["repo-a".to_string()], Some(resolution)).unwrap();

            let scoped_total: usize = scoped.communities.iter().map(|c| c.member_count).sum();
            assert_eq!(
                scoped_total, 4,
                "scoped output must cover exactly repo-a's 4 symbols"
            );
            assert_eq!(
                scoped.communities.len(),
                1,
                "repo-a's induced subgraph is a clique and must form one community"
            );
            for community in &scoped.communities {
                for member in &community.members {
                    assert!(
                        member.uid.starts_with('a'),
                        "a repo-scoped run must never return a repo-b symbol: {}",
                        member.uid
                    );
                }
            }
        }

        /// The disclosed count must equal exactly the edges that TOUCH the
        /// scope but are cut by the induced-subgraph boundary -- not edges
        /// wholly inside the scope (already included) and not edges wholly
        /// outside it (unrelated to this scope).
        #[test]
        fn clusters_repo_scope_discloses_excluded_cross_repo_edges() {
            let store = GraphStore::in_memory().unwrap();
            insert_test_repo(&store, "repo-a");
            insert_test_repo(&store, "repo-b");
            insert_test_repo(&store, "repo-c");

            store
                .insert_symbol(&make_symbol_in_repo("a0", "a_fn_0", "repo-a"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("a1", "a_fn_1", "repo-a"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("b0", "b_fn_0", "repo-b"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("c0", "c_fn_0", "repo-c"))
                .unwrap();

            // Intra-repo-a edge: part of the induced subgraph, not excluded.
            store.insert_edge(&make_edge("a0", "a1")).unwrap();
            // Two edges touching repo-a from outside: excluded.
            store.insert_edge(&make_edge("a0", "b0")).unwrap();
            store.insert_edge(&make_edge("a1", "c0")).unwrap();
            // An edge between two OTHER repos -- neither endpoint in
            // repo-a -- is unrelated to this scope and must NOT be counted.
            store.insert_edge(&make_edge("b0", "c0")).unwrap();

            let scoped =
                compute_clusters_scoped(&store, &["repo-a".to_string()], Some(1.0)).unwrap();

            assert_eq!(scoped.scope.cross_repo_edges_excluded, 2);
            assert_eq!(scoped.scope.repos, vec!["repo-a".to_string()]);
            assert_eq!(scoped.scope.id_space, "repo_scoped");
        }

        /// A scoped run must never write to the canonical sidecar --
        /// `hub_nodes`/`bridge_nodes`/`blast_radius`/`cluster <id>` (with no
        /// `--resolution`) all read it as "whatever was computed most
        /// recently for the WHOLE graph", and a partial-graph scoped
        /// partition silently becoming that answer would corrupt every one
        /// of those callers.
        #[test]
        fn clusters_repo_scope_never_writes_the_global_sidecar() {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("test.lbug");
            let store = GraphStore::open_or_create(&db_path).unwrap();
            insert_test_repo(&store, "repo-a");
            store
                .insert_symbol(&make_symbol_in_repo("a0", "a_fn_0", "repo-a"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("a1", "a_fn_1", "repo-a"))
                .unwrap();
            store.insert_edge(&make_edge("a0", "a1")).unwrap();

            // Seed the sidecar with a KNOWN unscoped run first, so "unchanged"
            // is a meaningful assertion rather than "still absent".
            let baseline = compute_clusters(&store, 1.0).unwrap();
            save_clusters(&db_path, &baseline, store.graph_generation()).unwrap();
            let sidecar = sidecar_path(&db_path);
            let before_bytes = fs::read(&sidecar).unwrap();
            let before_mtime = fs::metadata(&sidecar).unwrap().modified().unwrap();

            // The scoped call itself must not touch the sidecar -- it only
            // returns a `ScopedClusteringOutput`, which has no save path.
            let _scoped =
                compute_clusters_scoped(&store, &["repo-a".to_string()], Some(1.0)).unwrap();

            let after_bytes = fs::read(&sidecar).unwrap();
            let after_mtime = fs::metadata(&sidecar).unwrap().modified().unwrap();
            assert_eq!(
                before_bytes, after_bytes,
                "a scoped run must never touch the global sidecar's bytes"
            );
            assert_eq!(
                before_mtime, after_mtime,
                "a scoped run must never touch the global sidecar's mtime"
            );
        }

        /// COUNTERWEIGHT: the UNSCOPED path is still expected to write the
        /// sidecar -- proving the assertion above tests real behavior, not a
        /// `save_clusters` that has quietly stopped writing altogether.
        #[test]
        fn clusters_unscoped_still_writes_the_global_sidecar() {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("test.lbug");
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("a0", "a_fn_0", "repo-a"))
                .unwrap();

            assert!(!sidecar_path(&db_path).exists());
            let output = compute_clusters(&store, 1.0).unwrap();
            save_clusters(&db_path, &output, 1).unwrap();
            assert!(
                sidecar_path(&db_path).exists(),
                "the unscoped path must still write the canonical sidecar"
            );
        }

        /// `cluster_id` paging (the MCP/CLI layer filters `communities` by
        /// `id`, mirroring `tool_clusters`) must stay inside the scope: since
        /// a scoped `ScopedClusteringOutput` only ever contains communities
        /// computed from the induced subgraph, filtering it by id can never
        /// surface an out-of-scope member.
        #[test]
        fn clusters_repo_scope_cluster_id_paging_stays_in_scope() {
            let store = GraphStore::in_memory().unwrap();
            insert_test_repo(&store, "repo-a");
            insert_test_repo(&store, "repo-b");

            // repo-a: two disconnected pairs -> (at least) two communities.
            store
                .insert_symbol(&make_symbol_in_repo("a0", "a_fn_0", "repo-a"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("a1", "a_fn_1", "repo-a"))
                .unwrap();
            store.insert_edge(&make_edge("a0", "a1")).unwrap();
            store.insert_edge(&make_edge("a1", "a0")).unwrap();

            store
                .insert_symbol(&make_symbol_in_repo("a2", "a_fn_2", "repo-a"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("a3", "a_fn_3", "repo-a"))
                .unwrap();
            store.insert_edge(&make_edge("a2", "a3")).unwrap();
            store.insert_edge(&make_edge("a3", "a2")).unwrap();

            // repo-b: symbols the scope must never surface, even under
            // `cluster_id` paging.
            store
                .insert_symbol(&make_symbol_in_repo("b0", "b_fn_0", "repo-b"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("b1", "b_fn_1", "repo-b"))
                .unwrap();
            store.insert_edge(&make_edge("b0", "b1")).unwrap();
            store.insert_edge(&make_edge("b1", "b0")).unwrap();

            let scoped =
                compute_clusters_scoped(&store, &["repo-a".to_string()], Some(1.0)).unwrap();
            assert!(!scoped.communities.is_empty());

            // Page into ONE community by id, exactly the way `tool_clusters`
            // filters `output.communities` by `cluster_id`.
            let target_id = scoped.communities[0].id;
            let paged: Vec<&CommunityInfo> = scoped
                .communities
                .iter()
                .filter(|c| c.id == target_id)
                .collect();
            assert_eq!(paged.len(), 1);
            for member in &paged[0].members {
                assert!(
                    member.uid.starts_with('a'),
                    "cluster_id paging over a scoped result must stay inside \
                     the scope: {}",
                    member.uid
                );
            }
        }

        /// An unresolvable `--repo` selector is an ERROR, never a silent
        /// empty scope -- matching the hubs/bridges (nw-468) and dead_code
        /// (Task 4.7) precedent.
        #[test]
        fn clusters_unknown_repo_is_an_error() {
            let store = GraphStore::in_memory().unwrap();
            insert_test_repo(&store, "repo-a");
            store
                .insert_symbol(&make_symbol_in_repo("a0", "a_fn_0", "repo-a"))
                .unwrap();

            let err = compute_clusters_scoped(&store, &["repo-does-not-exist".to_string()], None)
                .unwrap_err();

            assert!(
                err.chain().any(|cause| cause
                    .downcast_ref::<crate::node_scope::RepoFilterUnresolved>()
                    .is_some()),
                "an unknown repo selector must surface as a typed \
                 RepoFilterUnresolved, got: {err:#}"
            );
        }

        /// COUNTERWEIGHT: `compute_clusters_scoped` requires a non-empty
        /// selector list -- an empty scope has no "induced by these repos"
        /// meaning; the caller wants `compute_clusters` for that.
        #[test]
        fn clusters_repo_scope_requires_at_least_one_repo() {
            let store = GraphStore::in_memory().unwrap();
            let err = compute_clusters_scoped(&store, &[], None).unwrap_err();
            assert!(
                err.to_string().contains("at least one repo selector"),
                "got: {err:#}"
            );
        }

        /// A resolved repo with zero `Symbol` rows must return an empty
        /// community list, not panic -- the induced subgraph is legitimately
        /// empty (`n == 0`) while the STORE as a whole is not, which is a
        /// different code path than the "the whole store has no symbols"
        /// early return: `scoped_indices` is empty, `global_to_local` is
        /// empty, and the edge loop's `(true, true)` arm must not be reached
        /// for any edge (repo-a owns none), only ever landing in the
        /// `(false, false)` arm for repo-b's own edge.
        #[test]
        fn clusters_repo_scope_with_a_repo_that_has_no_symbols_is_empty() {
            let store = GraphStore::in_memory().unwrap();
            insert_test_repo(&store, "repo-a");
            insert_test_repo(&store, "repo-b");

            // repo-a is a real, resolvable repo -- just an empty one.
            store
                .insert_symbol(&make_symbol_in_repo("b0", "b_fn_0", "repo-b"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("b1", "b_fn_1", "repo-b"))
                .unwrap();
            store.insert_edge(&make_edge("b0", "b1")).unwrap();

            let scoped =
                compute_clusters_scoped(&store, &["repo-a".to_string()], Some(1.0)).unwrap();

            assert!(scoped.communities.is_empty());
            assert_eq!(scoped.modularity, 0.0);
            assert_eq!(scoped.scope.repos, vec!["repo-a".to_string()]);
            assert_eq!(
                scoped.scope.cross_repo_edges_excluded, 0,
                "repo-b's own edge touches neither endpoint in repo-a"
            );
        }

        /// nw-479 Task 4.7b. Scoped community `id`s are NOT a global
        /// identifier: the CLI/MCP layer (pending, not this task) must
        /// refuse to resolve a scoped id against the global sidecar unless
        /// the same `repos` were supplied. This pins the two facts that make
        /// that refusal necessary: (1) every scoped output is tagged
        /// `id_space: "repo_scoped"`, and (2) the SAME numeric id can name
        /// entirely different members depending on which repos were scoped
        /// -- so an id alone, without `repos`, is not a safe lookup key.
        #[test]
        fn clusters_scoped_ids_do_not_resolve_without_repos() {
            let store = GraphStore::in_memory().unwrap();
            insert_test_repo(&store, "repo-a");
            insert_test_repo(&store, "repo-b");

            store
                .insert_symbol(&make_symbol_in_repo("a0", "a_fn_0", "repo-a"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("a1", "a_fn_1", "repo-a"))
                .unwrap();
            store.insert_edge(&make_edge("a0", "a1")).unwrap();
            store.insert_edge(&make_edge("a1", "a0")).unwrap();

            store
                .insert_symbol(&make_symbol_in_repo("b0", "b_fn_0", "repo-b"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("b1", "b_fn_1", "repo-b"))
                .unwrap();
            store.insert_edge(&make_edge("b0", "b1")).unwrap();
            store.insert_edge(&make_edge("b1", "b0")).unwrap();

            let scoped_a =
                compute_clusters_scoped(&store, &["repo-a".to_string()], Some(1.0)).unwrap();
            let scoped_b =
                compute_clusters_scoped(&store, &["repo-b".to_string()], Some(1.0)).unwrap();

            assert_eq!(scoped_a.scope.id_space, "repo_scoped");
            assert_eq!(scoped_b.scope.id_space, "repo_scoped");

            // Both scopes are structurally identical (one connected pair),
            // so they assign the SAME numeric id to their one community.
            assert_eq!(scoped_a.communities.len(), 1);
            assert_eq!(scoped_b.communities.len(), 1);
            let id_a = scoped_a.communities[0].id;
            let id_b = scoped_b.communities[0].id;
            assert_eq!(
                id_a, id_b,
                "premise: two structurally-identical two-node scopes must \
                 assign the same numeric id"
            );

            let members_a: HashSet<&str> = scoped_a.communities[0]
                .members
                .iter()
                .map(|m| m.uid.as_str())
                .collect();
            let members_b: HashSet<&str> = scoped_b.communities[0]
                .members
                .iter()
                .map(|m| m.uid.as_str())
                .collect();
            assert_ne!(
                members_a, members_b,
                "the SAME numeric id must name different members depending \
                 on which repos were scoped -- id alone, without `repos`, is \
                 unsafe to resolve"
            );
        }

        #[test]
        fn default_cluster_resolution_for_symbol_count_matches_the_threshold() {
            assert_eq!(
                default_cluster_resolution_for_symbol_count(0),
                SMALL_GRAPH_CLUSTER_RESOLUTION
            );
            assert_eq!(
                default_cluster_resolution_for_symbol_count(LARGE_GRAPH_SYMBOL_THRESHOLD),
                SMALL_GRAPH_CLUSTER_RESOLUTION
            );
            assert_eq!(
                default_cluster_resolution_for_symbol_count(LARGE_GRAPH_SYMBOL_THRESHOLD + 1),
                LARGE_GRAPH_CLUSTER_RESOLUTION
            );
        }

        /// The default resolution for a scoped run comes from the
        /// SUBGRAPH's size, not the whole store's -- and an explicit
        /// resolution still wins over that default.
        #[test]
        fn clusters_repo_scope_default_resolution_comes_from_subgraph_size() {
            let store = GraphStore::in_memory().unwrap();
            insert_test_repo(&store, "repo-a");
            store
                .insert_symbol(&make_symbol_in_repo("a0", "a_fn_0", "repo-a"))
                .unwrap();
            store
                .insert_symbol(&make_symbol_in_repo("a1", "a_fn_1", "repo-a"))
                .unwrap();
            store.insert_edge(&make_edge("a0", "a1")).unwrap();

            let scoped = compute_clusters_scoped(&store, &["repo-a".to_string()], None).unwrap();
            assert_eq!(scoped.resolution, SMALL_GRAPH_CLUSTER_RESOLUTION);

            let explicit =
                compute_clusters_scoped(&store, &["repo-a".to_string()], Some(2.0)).unwrap();
            assert_eq!(explicit.resolution, 2.0);
        }
    }
}
