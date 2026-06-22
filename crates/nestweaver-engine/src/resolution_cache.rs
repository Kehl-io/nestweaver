use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ResolutionCacheFile {
    version: u32,
    deps: HashMap<String, HashSet<String>>,
}

/// Tracks which files each file's resolved edges point to,
/// enabling incremental re-resolution when only a subset of files change.
pub struct ResolutionDeps {
    deps: HashMap<String, HashSet<String>>,
}

impl ResolutionDeps {
    /// Load resolution deps from a MessagePack file, returning empty deps on any error.
    pub fn load(path: &Path) -> Self {
        let deps = match std::fs::read(path) {
            Ok(data) => match rmp_serde::from_slice::<ResolutionCacheFile>(&data) {
                Ok(file) if file.version == CACHE_VERSION => file.deps,
                _ => HashMap::new(),
            },
            Err(_) => HashMap::new(),
        };
        Self { deps }
    }

    /// Persist resolution deps to a MessagePack file.
    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        let file = ResolutionCacheFile {
            version: CACHE_VERSION,
            deps: self.deps.clone(),
        };
        let data = rmp_serde::to_vec(&file).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
        std::fs::write(path, data).map_err(|e| anyhow::anyhow!("write: {e}"))?;
        Ok(())
    }

    /// Record the set of files that `file_path` depends on (has edges pointing to).
    pub fn set_deps(&mut self, file_path: String, depends_on: HashSet<String>) {
        self.deps.insert(file_path, depends_on);
    }

    /// Return changed files PLUS any file whose resolved edges depend on a changed file.
    pub fn affected_files(&self, changed: &HashSet<String>) -> HashSet<String> {
        let mut affected = changed.clone();
        for (file, deps) in &self.deps {
            if deps.iter().any(|d| changed.contains(d)) {
                affected.insert(file.clone());
            }
        }
        affected
    }

    /// True when no dependency information has been recorded yet (first run).
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty()
    }

    /// Evict entries for files that no longer exist in the repo.
    pub fn retain_files(&mut self, live_files: &std::collections::HashSet<String>) {
        self.deps.retain(|file, _| live_files.contains(file));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affected_files_includes_changed_and_dependents() {
        let mut deps = ResolutionDeps {
            deps: HashMap::new(),
        };
        // a.ts depends on b.ts and c.ts
        deps.set_deps(
            "a.ts".to_string(),
            HashSet::from(["b.ts".to_string(), "c.ts".to_string()]),
        );
        // d.ts depends on a.ts
        deps.set_deps("d.ts".to_string(), HashSet::from(["a.ts".to_string()]));
        // e.ts depends on nothing relevant
        deps.set_deps("e.ts".to_string(), HashSet::from(["f.ts".to_string()]));

        // b.ts changed
        let changed = HashSet::from(["b.ts".to_string()]);
        let affected = deps.affected_files(&changed);

        // b.ts (changed) + a.ts (depends on b.ts) should be affected
        assert!(affected.contains("b.ts"), "changed file must be affected");
        assert!(
            affected.contains("a.ts"),
            "file depending on changed file must be affected"
        );
        // d.ts depends on a.ts which isn't in `changed` — NOT affected
        assert!(
            !affected.contains("d.ts"),
            "transitive dependents should not be included"
        );
        assert!(
            !affected.contains("e.ts"),
            "unrelated file should not be affected"
        );
    }

    #[test]
    fn affected_files_returns_only_changed_when_no_deps() {
        let deps = ResolutionDeps {
            deps: HashMap::new(),
        };
        let changed = HashSet::from(["x.ts".to_string()]);
        let affected = deps.affected_files(&changed);
        assert_eq!(affected, changed);
    }

    #[test]
    fn round_trip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.resolution_deps.bin");

        let mut original = ResolutionDeps {
            deps: HashMap::new(),
        };
        original.set_deps(
            "a.rs".to_string(),
            HashSet::from(["b.rs".to_string(), "c.rs".to_string()]),
        );
        original.set_deps("d.rs".to_string(), HashSet::from(["a.rs".to_string()]));

        original.save(&path).expect("save should succeed");

        let loaded = ResolutionDeps::load(&path);
        assert_eq!(loaded.deps, original.deps);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let deps = ResolutionDeps::load(Path::new("/nonexistent/path.bin"));
        assert!(deps.is_empty());
    }

    #[test]
    fn is_empty_initially() {
        let deps = ResolutionDeps {
            deps: HashMap::new(),
        };
        assert!(deps.is_empty());
    }
}
