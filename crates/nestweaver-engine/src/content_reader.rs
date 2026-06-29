// content_reader.rs — abstracts how the indexer reads file contents and discovers files.
// `FilesystemReader` for local repos, `GitBareReader` for server-side bare clones (Task 6).

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result};

/// Abstracts how the indexer reads file contents and discovers files.
/// `FilesystemReader` preserves local behavior; `GitBareReader` (added in Task 6)
/// reads from blobless bare clones via a pooled, persistent `git cat-file --batch`
/// subprocess (one process per reader, reused for every file read).
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
                if e.file_type().is_some_and(|ft| ft.is_dir())
                    && let Some(name) = e.file_name().to_str()
                    && crate::index::SKIP_DIRS.contains(&name)
                {
                    return false;
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
            if entry.file_type().is_some_and(|ft| ft.is_file())
                && let Ok(rel) = entry.path().strip_prefix(&self.repo_path)
            {
                files.push(rel.to_path_buf());
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

/// One object resolved from the `git cat-file --batch` stream.
enum BatchObject {
    /// Object found — its full content as raw bytes.
    Found(Vec<u8>),
    /// Git reported `<spec> missing` — no such object/path at this revision.
    Missing,
}

/// A persistent, pooled `git cat-file --batch` subprocess.
///
/// Spawned once per [`GitBareReader`] (lazily, on the first read) and reused for
/// every file read, so a full index pass over an N-file repo forks a single git
/// process instead of N. Each request writes one `<sha>:<path>` line and reads
/// back the framed response (`<oid> <type> <size>\n`, then `<size>` bytes, then a
/// trailing newline).
struct CatFileBatch {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl CatFileBatch {
    /// Spawn `git -C <bare_path> cat-file --batch` with piped stdin/stdout.
    fn spawn(bare_path: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .args([
                "-C",
                &bare_path.display().to_string(),
                "cat-file",
                "--batch",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn git cat-file --batch")?;
        let stdin = child
            .stdin
            .take()
            .context("cat-file --batch child has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("cat-file --batch child has no stdout")?;
        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    /// Resolve one object by its `<sha>:<path>` spec.
    ///
    /// Returns `Ok(BatchObject::Missing)` when git reports the path missing.
    /// Returns `Err` only for I/O failures (the batch process has likely died),
    /// so the caller can fall back to a one-shot `git show`.
    fn request(&mut self, sha: &str, rel_path: &Path) -> Result<BatchObject> {
        // Send the request line: "<sha>:<path>\n".
        writeln!(self.stdin, "{}:{}", sha, rel_path.display())
            .context("write request to cat-file --batch")?;
        self.stdin.flush().context("flush cat-file --batch stdin")?;

        // Read the header: "<oid> <type> <size>\n" or "<spec> missing\n".
        let mut header = String::new();
        let n = self
            .stdout
            .read_line(&mut header)
            .context("read cat-file --batch header")?;
        if n == 0 {
            anyhow::bail!("cat-file --batch closed its output unexpectedly");
        }
        let header = header.trim_end_matches('\n');
        if header.ends_with(" missing") {
            return Ok(BatchObject::Missing);
        }

        // Object size is the final whitespace-separated field of the header.
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .with_context(|| format!("malformed cat-file --batch header: {header:?}"))?;

        // Read exactly `size` bytes of content, then consume the trailing newline.
        let mut content = vec![0u8; size];
        self.stdout
            .read_exact(&mut content)
            .context("read cat-file --batch object content")?;
        let mut newline = [0u8; 1];
        self.stdout
            .read_exact(&mut newline)
            .context("read cat-file --batch trailing newline")?;

        Ok(BatchObject::Found(content))
    }
}

impl Drop for CatFileBatch {
    fn drop(&mut self) {
        // Kill and reap the child so no zombie git process leaks.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads file contents from a bare git clone without a working tree.
///
/// Individual file reads go through a persistent, pooled `git cat-file --batch`
/// subprocess (spawned lazily on first read), falling back to a one-shot
/// `git show <sha>:<path>` if that process cannot be spawned or has died. File
/// listing uses `git ls-tree -r --name-only <sha>`. This avoids needing a
/// checkout — the server only needs transient access to blobs.
pub struct GitBareReader {
    bare_path: PathBuf,
    sha: String,
    /// Lazily-spawned pooled `cat-file --batch` process. `None` until the first
    /// read; reset to `None` if the process dies so the next read re-spawns.
    batch: Mutex<Option<CatFileBatch>>,
}

impl GitBareReader {
    pub fn new(bare_path: &Path, sha: &str) -> Self {
        Self {
            bare_path: bare_path.to_path_buf(),
            sha: sha.to_string(),
            batch: Mutex::new(None),
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

    /// One-shot fallback read used when the pooled `cat-file --batch` process is
    /// unavailable (failed to spawn, or died mid-stream).
    fn read_file_via_show(&self, rel_path: &Path) -> Result<String> {
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
}

impl ContentReader for GitBareReader {
    fn read_file(&self, rel_path: &Path) -> Result<String> {
        let mut guard = self.batch.lock().unwrap_or_else(|e| e.into_inner());

        // Lazily spawn the pooled batch process on the first read.
        if guard.is_none() {
            match CatFileBatch::spawn(&self.bare_path) {
                Ok(batch) => *guard = Some(batch),
                Err(err) => {
                    tracing::warn!(
                        "cat-file --batch spawn failed ({err}); falling back to git show"
                    );
                    drop(guard);
                    return self.read_file_via_show(rel_path);
                }
            }
        }

        let batch = guard.as_mut().expect("batch initialized above");
        match batch.request(&self.sha, rel_path) {
            Ok(BatchObject::Found(content)) => String::from_utf8(content)
                .with_context(|| format!("non-utf8 content in {}", rel_path.display())),
            Ok(BatchObject::Missing) => anyhow::bail!(
                "path {} not found at {} in {}",
                rel_path.display(),
                self.sha,
                self.bare_path.display()
            ),
            Err(err) => {
                // The batch process likely died — discard it (so the next read
                // re-spawns) and fall back to a one-shot `git show`.
                tracing::warn!("cat-file --batch read failed ({err}); falling back to git show");
                *guard = None;
                drop(guard);
                self.read_file_via_show(rel_path)
            }
        }
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

    fn file_meta(&self, _rel_path: &Path) -> Result<Option<(u64, u64)>> {
        // Bare repos have no filesystem mtime. Return None so callers
        // (tiered_change_check, index_md) use content-hash or always-process paths.
        Ok(None)
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
        assert!(
            meta.is_none(),
            "GitBareReader should return None (no filesystem mtime)"
        );
    }

    #[test]
    fn git_bare_reader_file_meta_missing() {
        let (_tmp, bare, sha) = setup_bare_repo(&[("a.txt", "x")]);
        let reader = GitBareReader::new(&bare, &sha);
        let meta = reader.file_meta(Path::new("missing.txt")).unwrap();
        assert!(
            meta.is_none(),
            "GitBareReader returns None for all paths (no filesystem mtime)"
        );
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

    #[test]
    fn git_bare_reader_reads_multiple_files_one_reader() {
        // The pooled cat-file --batch process must stay in sync across many
        // reads (including repeats and nested paths) through a single reader.
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("a.txt", "alpha"),
            ("dir/b.txt", "bravo"),
            ("dir/sub/c.txt", "charlie\nmultiline"),
        ]);
        let reader = GitBareReader::new(&bare, &sha);

        assert_eq!(reader.read_file(Path::new("a.txt")).unwrap(), "alpha");
        assert_eq!(reader.read_file(Path::new("dir/b.txt")).unwrap(), "bravo");
        assert_eq!(
            reader.read_file(Path::new("dir/sub/c.txt")).unwrap(),
            "charlie\nmultiline"
        );
        // Repeat reads return identical content — the persistent stream framing
        // is consumed exactly per request.
        assert_eq!(reader.read_file(Path::new("a.txt")).unwrap(), "alpha");
        assert_eq!(reader.read_file(Path::new("dir/b.txt")).unwrap(), "bravo");
    }

    #[test]
    fn git_bare_reader_missing_path_does_not_wedge_stream() {
        // A missing path must error cleanly without desyncing the batch stream,
        // so subsequent valid reads still succeed through the same reader.
        let (_tmp, bare, sha) = setup_bare_repo(&[("present.txt", "here")]);
        let reader = GitBareReader::new(&bare, &sha);

        assert_eq!(reader.read_file(Path::new("present.txt")).unwrap(), "here");
        assert!(reader.read_file(Path::new("absent.txt")).is_err());
        assert_eq!(reader.read_file(Path::new("present.txt")).unwrap(), "here");
    }

    #[test]
    fn git_bare_reader_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GitBareReader>();
    }
}
