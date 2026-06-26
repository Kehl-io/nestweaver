// content_reader.rs — abstracts how the indexer reads file contents and discovers files.
// `FilesystemReader` for local repos, `GitBareReader` for server-side bare clones (Task 6).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

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
        std::fs::read_to_string(&abs).map_err(|e| anyhow::anyhow!("read {}: {e}", abs.display()))
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

/// Reads file contents from a bare git clone without a working tree.
///
/// Uses `git show <sha>:<path>` for individual file reads and
/// `git ls-tree -r --name-only <sha>` for file listing. This avoids
/// needing a checkout — the server only needs transient access to blobs.
pub struct GitBareReader {
    bare_path: PathBuf,
    sha: String,
}

impl GitBareReader {
    pub fn new(bare_path: &Path, sha: &str) -> Self {
        Self {
            bare_path: bare_path.to_path_buf(),
            sha: sha.to_string(),
        }
    }

    /// Resolve HEAD of the bare repo to a full SHA.
    pub fn from_head(bare_path: &Path) -> Result<Self> {
        let output = Command::new("git")
            .args(["-C", &bare_path.display().to_string(), "rev-parse", "HEAD"])
            .output()
            .context("failed to run git rev-parse HEAD")?;
        if !output.status.success() {
            anyhow::bail!(
                "git rev-parse HEAD failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let sha = String::from_utf8(output.stdout)
            .context("non-utf8 SHA")?
            .trim()
            .to_string();
        Ok(Self::new(bare_path, &sha))
    }
}

impl ContentReader for GitBareReader {
    fn read_file(&self, rel_path: &Path) -> Result<String> {
        let spec = format!("{}:{}", self.sha, rel_path.display());
        let output = Command::new("git")
            .args(["-C", &self.bare_path.display().to_string(), "show", &spec])
            .output()
            .with_context(|| format!("failed to run git show {spec}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "git show {} failed: {}",
                spec,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout)
            .with_context(|| format!("non-utf8 content in {}", rel_path.display()))
    }

    fn list_files(&self) -> Result<Vec<PathBuf>> {
        let output = Command::new("git")
            .args([
                "-C",
                &self.bare_path.display().to_string(),
                "ls-tree",
                "-r",
                "--name-only",
                &self.sha,
            ])
            .output()
            .context("failed to run git ls-tree")?;
        if !output.status.success() {
            anyhow::bail!(
                "git ls-tree failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let text = String::from_utf8(output.stdout).context("non-utf8 ls-tree output")?;
        let files: Vec<PathBuf> = text
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .filter(|p| !crate::index::path_in_skip_dir(p))
            .collect();
        Ok(files)
    }

    fn file_meta(&self, rel_path: &Path) -> Result<Option<(u64, u64)>> {
        // Bare repos have no filesystem mtime. Return size only (mtime = 0).
        let spec = format!("{}:{}", self.sha, rel_path.display());
        let output = Command::new("git")
            .args([
                "-C",
                &self.bare_path.display().to_string(),
                "cat-file",
                "-s",
                &spec,
            ])
            .output()
            .with_context(|| format!("failed to run git cat-file -s {spec}"))?;
        if !output.status.success() {
            // File doesn't exist at this SHA — treat as missing.
            anyhow::bail!(
                "git cat-file -s {} failed: {}",
                spec,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let size: u64 = String::from_utf8(output.stdout)
            .context("non-utf8 cat-file output")?
            .trim()
            .parse()
            .context("invalid size from cat-file -s")?;
        // No mtime available in bare repos — return 0 for mtime.
        Ok(Some((0, size)))
    }

    fn root(&self) -> &Path {
        &self.bare_path
    }

    fn version_id(&self) -> &str {
        &self.sha
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
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
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
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"real.rs".to_string()));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
    }

    // ---------- GitBareReader tests ----------

    use std::process::Command;

    /// Helper: create a source repo with files, commit, and clone as bare.
    /// Returns (TempDir, bare_path, sha).
    fn setup_bare_repo(files: &[(&str, &str)]) -> (TempDir, PathBuf, String) {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src_repo");
        std::fs::create_dir_all(&src).unwrap();

        // Init repo.
        Command::new("git")
            .args(["init"])
            .current_dir(&src)
            .output()
            .unwrap();
        // Configure committer identity for CI environments.
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&src)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&src)
            .output()
            .unwrap();

        // Write files.
        for (path, content) in files {
            let full = src.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }

        // Stage and commit.
        Command::new("git")
            .args(["add", "."])
            .current_dir(&src)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&src)
            .output()
            .unwrap();

        // Clone as bare.
        let bare = tmp.path().join("repo.git");
        Command::new("git")
            .args([
                "clone",
                "--bare",
                &src.display().to_string(),
                &bare.display().to_string(),
            ])
            .output()
            .unwrap();

        // Get HEAD sha.
        let sha_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&bare)
            .output()
            .unwrap();
        let sha = String::from_utf8(sha_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        (tmp, bare, sha)
    }

    #[test]
    fn git_bare_reader_read_file() {
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("main.js", "function greet() {}"),
            ("lib/util.js", "export const x = 1;"),
        ]);
        let reader = GitBareReader::new(&bare, &sha);
        assert_eq!(
            reader.read_file(Path::new("main.js")).unwrap(),
            "function greet() {}"
        );
        assert_eq!(
            reader.read_file(Path::new("lib/util.js")).unwrap(),
            "export const x = 1;"
        );
    }

    #[test]
    fn git_bare_reader_missing_file() {
        let (_tmp, bare, sha) = setup_bare_repo(&[("a.txt", "hi")]);
        let reader = GitBareReader::new(&bare, &sha);
        assert!(reader.read_file(Path::new("nope.txt")).is_err());
    }

    #[test]
    fn git_bare_reader_list_files() {
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("src/lib.rs", ""),
            ("src/main.rs", "fn main() {}"),
            ("README.md", "# Hello"),
        ]);
        let reader = GitBareReader::new(&bare, &sha);
        let files = reader.list_files().unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"src/lib.rs".to_string()));
        assert!(names.contains(&"src/main.rs".to_string()));
        assert!(names.contains(&"README.md".to_string()));
    }

    #[test]
    fn git_bare_reader_list_files_skips_skip_dirs() {
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("src/lib.rs", ""),
            ("node_modules/foo/bar.js", "junk"),
            ("target/debug/x.rs", "junk"),
        ]);
        let reader = GitBareReader::new(&bare, &sha);
        let files = reader.list_files().unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"src/lib.rs".to_string()));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
        assert!(!names.iter().any(|n| n.contains("target")));
    }

    #[test]
    fn git_bare_reader_file_meta() {
        let (_tmp, bare, sha) = setup_bare_repo(&[("hello.txt", "world")]);
        let reader = GitBareReader::new(&bare, &sha);
        let meta = reader.file_meta(Path::new("hello.txt")).unwrap();
        assert!(meta.is_some());
        let (mtime, size) = meta.unwrap();
        // Bare repos return mtime=0.
        assert_eq!(mtime, 0);
        assert_eq!(size, 5); // "world" is 5 bytes
    }

    #[test]
    fn git_bare_reader_file_meta_missing() {
        let (_tmp, bare, sha) = setup_bare_repo(&[("a.txt", "x")]);
        let reader = GitBareReader::new(&bare, &sha);
        assert!(reader.file_meta(Path::new("missing.txt")).is_err());
    }

    #[test]
    fn git_bare_reader_root_and_version() {
        let (_tmp, bare, sha) = setup_bare_repo(&[("a.txt", "x")]);
        let reader = GitBareReader::new(&bare, &sha);
        assert_eq!(reader.root(), bare.as_path());
        assert_eq!(reader.version_id(), sha);
    }

    #[test]
    fn git_bare_reader_from_head() {
        let (_tmp, bare, sha) = setup_bare_repo(&[("a.txt", "x")]);
        let reader = GitBareReader::from_head(&bare).unwrap();
        assert_eq!(reader.version_id(), sha);
        assert_eq!(reader.read_file(Path::new("a.txt")).unwrap(), "x");
    }
}
