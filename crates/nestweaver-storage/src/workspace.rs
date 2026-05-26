use std::path::{Path, PathBuf};

pub struct WorkspaceStorage {
    root: PathBuf,
}

impl WorkspaceStorage {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn ensure_dir(&self, repo_name: &str) -> Result<PathBuf, anyhow::Error> {
        if repo_name.split('/').any(|c| c == "..") {
            anyhow::bail!("repo name '{}' contains '..' component", repo_name);
        }
        let path = self.root.join(repo_name);
        // Create the directory first so we can canonicalize it.
        std::fs::create_dir_all(&path)?;
        // Verify the resolved path is still under the workspace root.
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !canonical_path.starts_with(&canonical_root) {
            // Roll back the directory we just created, best-effort.
            let _ = std::fs::remove_dir_all(&path);
            anyhow::bail!("repo name '{}' would escape workspace root", repo_name);
        }
        Ok(path)
    }

    pub fn cleanup(&self, repo_name: &str) -> Result<(), anyhow::Error> {
        if repo_name.split('/').any(|c| c == "..") {
            anyhow::bail!("repo name '{}' contains '..' component", repo_name);
        }
        let dir = self.root.join(repo_name);
        // If the directory exists, use canonicalize to verify containment.
        if dir.exists() {
            let canonical_root = self
                .root
                .canonicalize()
                .unwrap_or_else(|_| self.root.clone());
            let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if !canonical_dir.starts_with(&canonical_root) {
                anyhow::bail!("repo name '{}' would escape workspace root", repo_name);
            }
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    pub fn ensure_gitignore(&self) -> Result<(), anyhow::Error> {
        let gitignore = self.root.join(".gitignore");
        std::fs::create_dir_all(&self.root)?;
        std::fs::write(gitignore, "*\n")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_dir_creates_repo_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceStorage::new(dir.path());
        let path = ws.ensure_dir("my-repo").unwrap();
        assert!(path.exists());
        assert!(path.ends_with("my-repo"));
    }

    #[test]
    fn cleanup_removes_repo_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceStorage::new(dir.path());
        ws.ensure_dir("my-repo").unwrap();
        ws.cleanup("my-repo").unwrap();
        assert!(!dir.path().join("my-repo").exists());
    }

    #[test]
    fn ensure_dir_rejects_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceStorage::new(dir.path());
        assert!(ws.ensure_dir("../escaped").is_err());
        assert!(ws.ensure_dir("sub/../../../escaped").is_err());
    }

    #[test]
    fn cleanup_rejects_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceStorage::new(dir.path());
        assert!(ws.cleanup("../escaped").is_err());
    }

    #[test]
    fn ensure_gitignore_writes_star() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceStorage::new(dir.path());
        ws.ensure_gitignore().unwrap();
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content.trim(), "*");
    }
}
