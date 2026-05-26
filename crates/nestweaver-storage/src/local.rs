use crate::backend::{SnapshotMeta, StorageBackend};
use std::path::{Path, PathBuf};

pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl StorageBackend for LocalBackend {
    fn push_snapshot(&self, src: &Path, meta: &SnapshotMeta) -> Result<(), anyhow::Error> {
        let version_dir = self.root.join(format!("v{}", meta.version));
        std::fs::create_dir_all(&version_dir)?;

        // Copy all files from src into the versioned directory
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_file() {
                let dest = version_dir.join(entry.file_name());
                std::fs::copy(entry.path(), dest)?;
            }
        }

        // Write meta.json alongside
        let meta_path = version_dir.join("meta.json");
        let meta_bytes = serde_json::to_vec_pretty(meta)?;
        std::fs::write(meta_path, meta_bytes)?;

        Ok(())
    }

    fn pull_snapshot(&self, dest: &Path) -> Result<SnapshotMeta, anyhow::Error> {
        // Find latest version by sorting versioned directories
        let latest = find_latest_version_dir(&self.root)?
            .ok_or_else(|| anyhow::anyhow!("no snapshots found in {:?}", self.root))?;

        // Read meta
        let meta_path = latest.join("meta.json");
        let meta_bytes = std::fs::read(&meta_path)?;
        let meta: SnapshotMeta = serde_json::from_slice(&meta_bytes)?;

        // Copy all files except meta.json to dest
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(&latest)? {
            let entry = entry?;
            if entry.file_name() == "meta.json" {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_file() {
                let target = dest.join(entry.file_name());
                std::fs::copy(entry.path(), target)?;
            }
        }

        Ok(meta)
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, anyhow::Error> {
        if !self.root.exists() {
            return Ok(vec![]);
        }

        let mut metas = Vec::new();
        let mut version_dirs: Vec<PathBuf> = std::fs::read_dir(&self.root)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                    && e.file_name().to_string_lossy().starts_with('v')
            })
            .map(|e| e.path())
            .collect();

        version_dirs.sort();

        for dir in version_dirs {
            let meta_path = dir.join("meta.json");
            if meta_path.exists() {
                let bytes = std::fs::read(&meta_path)?;
                let meta: SnapshotMeta = serde_json::from_slice(&bytes)?;
                metas.push(meta);
            }
        }

        Ok(metas)
    }
}

/// Returns the path of the directory with the lexicographically greatest version prefix.
fn find_latest_version_dir(root: &Path) -> Result<Option<PathBuf>, anyhow::Error> {
    if !root.exists() {
        return Ok(None);
    }

    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                && e.file_name().to_string_lossy().starts_with('v')
        })
        .map(|e| e.path())
        .collect();

    dirs.sort();
    Ok(dirs.into_iter().last())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pull_snapshot() {
        let src_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        std::fs::write(src_dir.path().join("graph.lbug"), b"fake db").unwrap();
        std::fs::write(src_dir.path().join("stamp.json"), b"{}").unwrap();

        let backend = LocalBackend::new(store_dir.path());
        let meta = SnapshotMeta {
            version: "0.1.0".into(),
            instance_id: "test".into(),
        };

        backend.push_snapshot(src_dir.path(), &meta).unwrap();
        let pulled = backend.pull_snapshot(dest_dir.path()).unwrap();
        assert_eq!(pulled.instance_id, "test");
        assert!(dest_dir.path().join("graph.lbug").exists());
    }

    #[test]
    fn list_snapshots_empty() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(dir.path());
        assert!(backend.list_snapshots().unwrap().is_empty());
    }

    #[test]
    fn list_snapshots_after_push() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("graph.lbug"), b"data").unwrap();

        let backend = LocalBackend::new(store.path());
        backend
            .push_snapshot(
                src.path(),
                &SnapshotMeta {
                    version: "0.1.0".into(),
                    instance_id: "t".into(),
                },
            )
            .unwrap();
        let list = backend.list_snapshots().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].version, "0.1.0");
    }

    #[test]
    fn pull_latest_when_multiple_versions() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        std::fs::write(src.path().join("graph.lbug"), b"v1").unwrap();
        let backend = LocalBackend::new(store.path());

        backend
            .push_snapshot(
                src.path(),
                &SnapshotMeta {
                    version: "0.1.0".into(),
                    instance_id: "first".into(),
                },
            )
            .unwrap();

        std::fs::write(src.path().join("graph.lbug"), b"v2").unwrap();
        backend
            .push_snapshot(
                src.path(),
                &SnapshotMeta {
                    version: "0.2.0".into(),
                    instance_id: "second".into(),
                },
            )
            .unwrap();

        let pulled = backend.pull_snapshot(dest.path()).unwrap();
        assert_eq!(pulled.instance_id, "second");
        assert_eq!(pulled.version, "0.2.0");
        // Confirm the actual file content is from v2
        let content = std::fs::read(dest.path().join("graph.lbug")).unwrap();
        assert_eq!(content, b"v2");
    }
}
