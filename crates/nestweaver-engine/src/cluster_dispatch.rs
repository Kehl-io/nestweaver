use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use nestweaver_store::GraphStore;

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
    if store.count_symbols().unwrap_or(0) > LARGE_GRAPH_SYMBOL_THRESHOLD {
        LARGE_GRAPH_CLUSTER_RESOLUTION
    } else {
        SMALL_GRAPH_CLUSTER_RESOLUTION
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
    let resolution = if resolution.is_finite() && resolution > 0.0 {
        resolution
    } else {
        1.0
    };
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

    // Build community output from the result.
    let mut communities: Vec<CommunityInfo> = Vec::new();
    for community in &result.communities {
        let members: Vec<ClusterMember> = community
            .members
            .iter()
            .map(|&idx| ClusterMember {
                uid: symbols[idx].uid.clone(),
                name: symbols[idx].name.clone(),
                file_path: symbols[idx].file_path.clone(),
                kind: symbols[idx].kind.clone(),
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

        communities.push(CommunityInfo {
            id: community.id,
            name,
            cohesion: community.cohesion,
            member_count: members.len(),
            members,
            key_files,
        });
    }

    // Sort communities by size descending.
    communities.sort_by_key(|c| std::cmp::Reverse(c.member_count));

    Ok(ClusteringOutput {
        resolution,
        modularity: result.modularity,
        communities,
    })
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

/// Atomically write `output` as JSON to `path`.
///
/// Writes to a process-unique temp file and renames into place, so a
/// concurrent reader (e.g. `hub_nodes` racing a `clusters` call) never
/// observes a partially-written file.
fn write_clusters_atomic(path: &Path, output: &ClusteringOutput) -> Result<()> {
    let json =
        serde_json::to_string_pretty(output).context("failed to serialize clustering output")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, json).with_context(|| format!("failed to write {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to move {} into place", path.display()));
    }
    Ok(())
}

/// Persist clustering output to the sidecar file(s).
///
/// nw-401: writes BOTH the canonical last-writer-wins path (unchanged
/// behavior, for callers that want "whatever was computed most recently") AND
/// a resolution-keyed copy (so a caller that later pins `--resolution` can
/// still find THIS run's data, undisturbed by a later run at a different
/// resolution). Concurrent writers at the SAME resolution still resolve to
/// last-writer-wins on the keyed path too — acceptable, because the output is
/// deterministic for a given graph state and resolution.
pub fn save_clusters(db_path: &Path, output: &ClusteringOutput) -> Result<()> {
    write_clusters_atomic(&sidecar_path(db_path), output)?;
    write_clusters_atomic(
        &sidecar_path_for_resolution(db_path, output.resolution),
        output,
    )?;
    Ok(())
}

/// Load a clustering output from a specific sidecar file path.
fn load_clusters_from(path: &Path) -> Result<Option<ClusteringOutput>> {
    if !path.exists() {
        return Ok(None);
    }
    let json =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let output: ClusteringOutput =
        serde_json::from_str(&json).context("failed to parse clusters sidecar")?;
    Ok(Some(output))
}

/// Load clustering output from the canonical (unkeyed) sidecar file, if it
/// exists.
///
/// Returns `Ok(None)` when the sidecar does not exist (i.e. clusters have
/// never been computed for this database). This is "whatever was computed
/// most recently, at whichever resolution" — the same last-writer-wins
/// semantics this function has always had. Callers that need a SPECIFIC
/// resolution, immune to a later differently-resolved run, must use
/// [`load_clusters_for_resolution`] instead.
pub fn load_clusters(db_path: &Path) -> Result<Option<ClusteringOutput>> {
    load_clusters_from(&sidecar_path(db_path))
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
    load_clusters_from(&sidecar_path_for_resolution(db_path, resolution))
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
        save_clusters(&db_path, &low).unwrap();

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
        save_clusters(&db_path, &high).unwrap();

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
            save_clusters(&db_path, &output).unwrap();
            let loaded = load_clusters(&db_path).unwrap().unwrap();
            assert_eq!(loaded.resolution, output.resolution);
        }
    }
}
