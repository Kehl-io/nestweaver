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

/// Run Leiden community detection on the code graph.
///
/// Loads all Symbol nodes and code-level edges (CALLS, IMPORTS, EXTENDS_SYM,
/// IMPLEMENTS_SYM, MEMBER_OF) from the store, builds an undirected weighted
/// graph, runs the Leiden algorithm, and returns structured output.
pub fn compute_clusters(store: &GraphStore, resolution: f64) -> Result<ClusteringOutput> {
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

    // Run Leiden clustering.
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
pub fn sidecar_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".clusters.json")
}

/// Persist clustering output to the sidecar file.
///
/// Writes to a process-unique temp file and renames into place, so a
/// concurrent `load_clusters` (e.g. `hub_nodes` racing a `clusters` call)
/// never observes a partially-written file. Concurrent writers resolve to
/// last-writer-wins — acceptable because the output is deterministic for a
/// given graph state.
pub fn save_clusters(db_path: &Path, output: &ClusteringOutput) -> Result<()> {
    let path = sidecar_path(db_path);
    let json =
        serde_json::to_string_pretty(output).context("failed to serialize clustering output")?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, json).with_context(|| format!("failed to write {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to move {} into place", path.display()));
    }
    Ok(())
}

/// Load clustering output from the sidecar file, if it exists.
///
/// Returns `Ok(None)` when the sidecar does not exist (i.e. clusters have
/// never been computed for this database).
pub fn load_clusters(db_path: &Path) -> Result<Option<ClusteringOutput>> {
    let path = sidecar_path(db_path);
    if !path.exists() {
        return Ok(None);
    }
    let json =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let output: ClusteringOutput =
        serde_json::from_str(&json).context("failed to parse clusters sidecar")?;
    Ok(Some(output))
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
}
