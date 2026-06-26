// content_reader.rs — abstracts how the indexer reads file contents and discovers files.
// `FilesystemReader` for local repos, `GitBareReader` for server-side bare clones (Task 6).

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Abstracts how the indexer reads file contents and discovers files.
/// `FilesystemReader` preserves local behavior; `GitBareReader` (added in Task 6)
/// reads from blobless bare clones via pooled `git cat-file --batch`.
pub trait ContentReader: Send + Sync {
    /// Read the full content of a file at `rel_path` (repo-relative).
    fn read_file(&self, rel_path: &Path) -> Result<String>;

    /// List all files in the repo (repo-relative paths), respecting
    /// gitignore and skip-dir rules.
    fn list_files(&self) -> Result<Vec<PathBuf>>;

    /// Return filesystem metadata for change detection.
    /// For FilesystemReader: `Some((mtime_secs, size_bytes))`.
    /// For GitBareReader: returns `None` (uses commit SHA instead of mtime).
    fn file_meta(&self, rel_path: &Path) -> Result<Option<(u64, u64)>>;

    /// The root path (for constructing absolute paths in parsers that need them).
    fn root(&self) -> &Path;

    /// An identifier for the content version (HEAD SHA for git, "local" for filesystem).
    fn version_id(&self) -> &str;
}

/// Local filesystem reader — wraps the existing `ignore::WalkBuilder` + `fs::read_to_string`.
pub struct FilesystemReader {
    repo_path: PathBuf,
}

impl FilesystemReader {
    pub fn new(repo_path: &Path) -> Self {
        Self {
            repo_path: repo_path.to_path_buf(),
        }
    }
}

impl ContentReader for FilesystemReader {
    fn read_file(&self, rel_path: &Path) -> Result<String> {
        let abs = self.repo_path.join(rel_path);
        std::fs::read_to_string(&abs)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", abs.display()))
    }

    fn list_files(&self) -> Result<Vec<PathBuf>> {
        use ignore::WalkBuilder;

        let mut files = Vec::new();
        let walker = WalkBuilder::new(&self.repo_path)
            .follow_links(false)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .filter_entry(|e| {
                if e.file_type().is_some_and(|ft| ft.is_dir()) {
                    if let Some(name) = e.file_name().to_str() {
                        if crate::index::SKIP_DIRS.contains(&name) {
                            return false;
                        }
                    }
                }
                true
            })
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    tracing::warn!("walk error: {err}");
                    continue;
                }
            };
            if entry.file_type().map_or(false, |ft| ft.is_file()) {
                if let Ok(rel) = entry.path().strip_prefix(&self.repo_path) {
                    files.push(rel.to_path_buf());
                }
            }
        }
        Ok(files)
    }

    fn file_meta(&self, rel_path: &Path) -> Result<Option<(u64, u64)>> {
        let abs = self.repo_path.join(rel_path);
        let meta = std::fs::metadata(&abs)?;
        let mtime = meta
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(Some((mtime, meta.len())))
    }

    fn root(&self) -> &Path {
        &self.repo_path
    }

    fn version_id(&self) -> &str {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn filesystem_reader_read_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("hello.rs"), "fn main() {}").unwrap();
        let reader = FilesystemReader::new(dir.path());
        let content = reader.read_file(Path::new("hello.rs")).unwrap();
        assert_eq!(content, "fn main() {}");
    }

    #[test]
    fn filesystem_reader_read_missing_file_errors() {
        let dir = TempDir::new().unwrap();
        let reader = FilesystemReader::new(dir.path());
        assert!(reader.read_file(Path::new("nope.rs")).is_err());
    }

    #[test]
    fn filesystem_reader_list_files() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        let reader = FilesystemReader::new(dir.path());
        let files = reader.list_files().unwrap();
        assert!(files.len() >= 2);
        // Verify both files are present (order-independent).
        let names: Vec<String> = files.iter().map(|p| p.to_string_lossy().to_string()).collect();
        assert!(names.contains(&"src/lib.rs".to_string()));
        assert!(names.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn filesystem_reader_file_meta() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.rs"), "hello").unwrap();
        let reader = FilesystemReader::new(dir.path());
        let meta = reader.file_meta(Path::new("test.rs")).unwrap();
        assert!(meta.is_some());
        let (mtime, size) = meta.unwrap();
        assert!(mtime > 0);
        assert_eq!(size, 5);
    }

    #[test]
    fn filesystem_reader_file_exists_false_for_missing() {
        let dir = TempDir::new().unwrap();
        let reader = FilesystemReader::new(dir.path());
        let meta = reader.file_meta(Path::new("missing.rs"));
        assert!(meta.is_err());
    }

    #[test]
    fn filesystem_reader_root_and_version() {
        let dir = TempDir::new().unwrap();
        let reader = FilesystemReader::new(dir.path());
        assert_eq!(reader.root(), dir.path());
        assert_eq!(reader.version_id(), "local");
    }

    #[test]
    fn filesystem_reader_skips_node_modules() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/foo")).unwrap();
        std::fs::write(dir.path().join("node_modules/foo/bar.js"), "").unwrap();
        std::fs::write(dir.path().join("real.rs"), "").unwrap();
        let reader = FilesystemReader::new(dir.path());
        let files = reader.list_files().unwrap();
        let names: Vec<String> = files.iter().map(|p| p.to_string_lossy().to_string()).collect();
        assert!(names.contains(&"real.rs".to_string()));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
    }
}
