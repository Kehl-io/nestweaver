use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Sidecar format version. v2 = per-repo keying (nw-045). v1 (a flat,
/// bare-rel-path `deps` map shared across all repos) fails deserialization
/// and/or the version check and loads as empty — a deliberate fail-open that
/// costs one full re-resolution and can never resurrect a stale cross-repo
/// edge decision. Mirrors `index::FILEMETA_VERSION`.
const CACHE_VERSION: u32 = 2;

/// Maximum number of transitive reverse-dependency hops expanded when deciding
/// which files to re-resolve after a change. Shared by the local cache-based
/// path ([`ResolutionDeps::affected_files`]) and the server graph-based path
/// (`index::collect_reverse_dep_files`) so both honor the same blast bound.
pub const MAX_HOPS: usize = 2;

/// On-disk shape of `<db>.resolution_deps.bin`: dependency records keyed by
/// repo uid, then repo-relative path. Two repos sharing one DB can never
/// collide on a relative path (nw-045).
#[derive(Debug, Serialize, Deserialize)]
struct ResolutionCacheFile {
    version: u32,
    repos: HashMap<String, HashMap<String, HashSet<String>>>,
}

/// Tracks which files each file's resolved edges point to, sliced per repo so
/// two repos sharing one DB can't clobber each other's records on a shared
/// relative path. Enables incremental re-resolution when only a subset of
/// files change.
#[derive(Default)]
pub struct ResolutionDeps {
    /// repo_uid → (rel-path → set of rel-paths it depends on).
    repos: HashMap<String, HashMap<String, HashSet<String>>>,
}

impl ResolutionDeps {
    /// Load resolution deps from a MessagePack file. Missing, corrupt, or
    /// old-format (flat, pre-nw-045) files yield empty deps — a deliberate
    /// fail-open that costs one full re-resolution and never resurrects a
    /// stale cross-repo edge decision.
    pub fn load(path: &Path) -> Self {
        let repos = match std::fs::read(path) {
            Ok(data) => match rmp_serde::from_slice::<ResolutionCacheFile>(&data) {
                Ok(file) if file.version == CACHE_VERSION => file.repos,
                _ => HashMap::new(),
            },
            Err(_) => HashMap::new(),
        };
        Self { repos }
    }

    /// Persist resolution deps to a MessagePack file.
    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        let file = ResolutionCacheFile {
            version: CACHE_VERSION,
            repos: self.repos.clone(),
        };
        let data = rmp_serde::to_vec(&file).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
        std::fs::write(path, data).map_err(|e| anyhow::anyhow!("write: {e}"))?;
        Ok(())
    }

    /// Record, within `r_uid`'s slice, the set of files that `file_path`
    /// depends on (has edges pointing to). Never touches another repo's slice.
    pub fn set_deps_for_repo(
        &mut self,
        r_uid: &str,
        file_path: impl Into<String>,
        depends_on: HashSet<String>,
    ) {
        self.repos
            .entry(r_uid.to_string())
            .or_default()
            .insert(file_path.into(), depends_on);
    }

    /// Return changed files PLUS any file WITHIN `r_uid`'s slice whose resolved
    /// edges depend on a changed file, expanding up to `MAX_HOPS` levels of
    /// transitive dependents. This ensures that when file A changes and B
    /// depends on A, files that depend on B are also re-resolved (since B's
    /// exports may have changed shape).
    ///
    /// Iterates ONLY the current repo's slice, so another repo's record for
    /// the same relative path can never bleed into the decision (nw-045).
    /// Capped at [`MAX_HOPS`] iterations to prevent cascading through the graph.
    pub fn affected_files_for_repo(
        &self,
        r_uid: &str,
        changed: &HashSet<String>,
    ) -> HashSet<String> {
        let mut affected = changed.clone();
        let Some(slice) = self.repos.get(r_uid) else {
            return affected;
        };
        for _ in 0..MAX_HOPS {
            let mut newly_added = Vec::new();
            for (file, deps) in slice {
                if !affected.contains(file) && deps.iter().any(|d| affected.contains(d)) {
                    newly_added.push(file.clone());
                }
            }
            if newly_added.is_empty() {
                break; // fixed point — no more transitive dependents
            }
            affected.extend(newly_added);
        }
        affected
    }

    /// True when no dependency information has been recorded for any repo.
    pub fn is_empty(&self) -> bool {
        self.repos.values().all(|slice| slice.is_empty())
    }

    /// True when no dependency information has been recorded for `r_uid` (this
    /// repo's first resolution run). Gates the incremental-vs-full decision
    /// per repo so a fresh repo isn't treated as incremental just because some
    /// OTHER repo has prior data.
    pub fn is_empty_for_repo(&self, r_uid: &str) -> bool {
        self.repos.get(r_uid).is_none_or(|slice| slice.is_empty())
    }

    /// Evict entries for files that no longer exist in `r_uid`'s repo. Only
    /// this repo's slice is retained against `live_files`; other repos are
    /// untouched (nw-045 — the flat map previously forced a cross-repo union).
    pub fn retain_files_for_repo(&mut self, r_uid: &str, live_files: &HashSet<String>) {
        if let Some(slice) = self.repos.get_mut(r_uid) {
            slice.retain(|file, _| live_files.contains(file));
        }
    }

    /// Drop `r_uid`'s entire slice. Returns `true` if a slice existed. Only the
    /// named repo's slice is removed — every other repo sharing the DB keeps its
    /// records. Used by `remove-repo` (nw-048/nw-045) so a re-added repo starts
    /// from a clean dependency slice instead of inheriting a stale one.
    pub fn remove_repo(&mut self, r_uid: &str) -> bool {
        self.repos.remove(r_uid).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: &str = "repo:x";

    #[test]
    fn affected_files_includes_changed_and_dependents() {
        let mut deps = ResolutionDeps::default();
        // a.ts depends on b.ts and c.ts
        deps.set_deps_for_repo(
            R,
            "a.ts",
            HashSet::from(["b.ts".to_string(), "c.ts".to_string()]),
        );
        // d.ts depends on a.ts
        deps.set_deps_for_repo(R, "d.ts", HashSet::from(["a.ts".to_string()]));
        // e.ts depends on nothing relevant
        deps.set_deps_for_repo(R, "e.ts", HashSet::from(["f.ts".to_string()]));

        // b.ts changed
        let changed = HashSet::from(["b.ts".to_string()]);
        let affected = deps.affected_files_for_repo(R, &changed);

        // b.ts (changed) + a.ts (depends on b.ts) should be affected
        assert!(affected.contains("b.ts"), "changed file must be affected");
        assert!(
            affected.contains("a.ts"),
            "file depending on changed file must be affected"
        );
        // d.ts depends on a.ts — now included via 2-hop transitive expansion
        assert!(
            affected.contains("d.ts"),
            "2-hop transitive dependents should be included"
        );
        assert!(
            !affected.contains("e.ts"),
            "unrelated file should not be affected"
        );
    }

    #[test]
    fn affected_files_two_hop_chain() {
        let mut deps = ResolutionDeps::default();
        // Chain: a.ts -> b.ts -> c.ts -> d.ts
        deps.set_deps_for_repo(R, "b.ts", HashSet::from(["a.ts".to_string()]));
        deps.set_deps_for_repo(R, "c.ts", HashSet::from(["b.ts".to_string()]));
        deps.set_deps_for_repo(R, "d.ts", HashSet::from(["c.ts".to_string()]));

        let changed = HashSet::from(["a.ts".to_string()]);
        let affected = deps.affected_files_for_repo(R, &changed);

        // a.ts changed, b.ts is 1 hop, c.ts is 2 hops
        assert!(affected.contains("a.ts"));
        assert!(affected.contains("b.ts"), "1-hop dependent");
        assert!(affected.contains("c.ts"), "2-hop dependent");
        // d.ts is 3 hops — beyond the MAX_HOPS=2 cap
        assert!(
            !affected.contains("d.ts"),
            "3-hop dependent should be excluded by cap"
        );
    }

    #[test]
    fn affected_files_stops_at_fixed_point() {
        let mut deps = ResolutionDeps::default();
        // a.ts depends on b.ts, nothing else
        deps.set_deps_for_repo(R, "a.ts", HashSet::from(["b.ts".to_string()]));

        let changed = HashSet::from(["b.ts".to_string()]);
        let affected = deps.affected_files_for_repo(R, &changed);

        assert_eq!(affected.len(), 2); // b.ts + a.ts
        assert!(affected.contains("b.ts"));
        assert!(affected.contains("a.ts"));
    }

    #[test]
    fn affected_files_returns_only_changed_when_no_deps() {
        let deps = ResolutionDeps::default();
        let changed = HashSet::from(["x.ts".to_string()]);
        let affected = deps.affected_files_for_repo(R, &changed);
        assert_eq!(affected, changed);
    }

    #[test]
    fn round_trip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.resolution_deps.bin");

        let mut original = ResolutionDeps::default();
        original.set_deps_for_repo(
            R,
            "a.rs",
            HashSet::from(["b.rs".to_string(), "c.rs".to_string()]),
        );
        original.set_deps_for_repo(R, "d.rs", HashSet::from(["a.rs".to_string()]));
        // A second repo's slice must survive the round trip intact.
        original.set_deps_for_repo("repo:y", "a.rs", HashSet::from(["z.rs".to_string()]));

        original.save(&path).expect("save should succeed");

        let loaded = ResolutionDeps::load(&path);
        assert_eq!(loaded.repos, original.repos);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let deps = ResolutionDeps::load(Path::new("/nonexistent/path.bin"));
        assert!(deps.is_empty());
    }

    #[test]
    fn is_empty_initially() {
        let deps = ResolutionDeps::default();
        assert!(deps.is_empty());
    }

    #[test]
    fn set_deps_does_not_clobber_another_repos_same_rel_path() {
        let mut rd = ResolutionDeps::default();
        // repo A: src/utils.js depends on src/a.js
        rd.set_deps_for_repo(
            "repo:a",
            "src/utils.js",
            ["src/a.js"].into_iter().map(String::from).collect(),
        );
        // repo B: same rel path, different deps
        rd.set_deps_for_repo(
            "repo:b",
            "src/utils.js",
            ["src/b.js"].into_iter().map(String::from).collect(),
        );
        // A's entry must survive: affected_files for repo A when src/a.js
        // changed must flag src/utils.js
        let affected_a =
            rd.affected_files_for_repo("repo:a", &["src/a.js".to_string()].into_iter().collect());
        assert!(
            affected_a.contains("src/utils.js"),
            "repo A's dep record must survive repo B's set_deps"
        );
        // and B's when src/b.js changed
        let affected_b =
            rd.affected_files_for_repo("repo:b", &["src/b.js".to_string()].into_iter().collect());
        assert!(affected_b.contains("src/utils.js"));
        // cross: src/a.js changing must NOT flag repo B's src/utils.js
        let affected_b_cross =
            rd.affected_files_for_repo("repo:b", &["src/a.js".to_string()].into_iter().collect());
        assert!(
            !affected_b_cross.contains("src/utils.js"),
            "repo A's dep must not bleed into repo B"
        );
    }

    #[test]
    fn old_format_bin_loads_empty() {
        // An old flat-format (or otherwise corrupt) .bin must fail-open to an
        // empty tracker — a one-time full re-resolution, never a panic.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.resolution_deps.bin");

        // Simulate the pre-nw-045 on-disk shape: version 1 + a flat bare-keyed
        // `deps` map. The current loader must reject it (version mismatch / shape
        // mismatch) and yield empty deps.
        #[derive(Serialize)]
        struct LegacyFlatFile {
            version: u32,
            deps: HashMap<String, HashSet<String>>,
        }
        let mut deps = HashMap::new();
        deps.insert(
            "src/utils.js".to_string(),
            HashSet::from(["src/a.js".to_string()]),
        );
        let legacy = LegacyFlatFile { version: 1, deps };
        let data = rmp_serde::to_vec(&legacy).unwrap();
        std::fs::write(&path, data).unwrap();

        let loaded = ResolutionDeps::load(&path);
        assert!(
            loaded.is_empty(),
            "old-format .bin must load as empty (fail-open full re-resolution)"
        );
    }

    #[test]
    fn retain_files_for_repo_only_evicts_its_own_slice() {
        let mut rd = ResolutionDeps::default();
        rd.set_deps_for_repo("repo:a", "keep.js", HashSet::from(["dep.js".to_string()]));
        rd.set_deps_for_repo("repo:a", "gone.js", HashSet::from(["dep.js".to_string()]));
        rd.set_deps_for_repo("repo:b", "keep.js", HashSet::from(["dep.js".to_string()]));

        // Evict from repo A only: keep.js lives, gone.js is dropped.
        rd.retain_files_for_repo("repo:a", &HashSet::from(["keep.js".to_string()]));

        // repo A: gone.js no longer flags as a dependent.
        let a = rd.affected_files_for_repo("repo:a", &HashSet::from(["dep.js".to_string()]));
        assert!(a.contains("keep.js"));
        assert!(!a.contains("gone.js"), "repo A's gone.js must be evicted");
        // repo B untouched by repo A's retention.
        let b = rd.affected_files_for_repo("repo:b", &HashSet::from(["dep.js".to_string()]));
        assert!(b.contains("keep.js"), "repo B's slice must be untouched");
    }
}
