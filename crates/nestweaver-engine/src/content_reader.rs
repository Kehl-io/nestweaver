// content_reader.rs — abstracts how the indexer reads file contents and discovers files.
// `FilesystemReader` for local repos, `GitBareReader` for server-side bare clones (Task 6).

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::git_cmd::{apply_git_isolation, git_net_timeout, run_git_with_timeout};
use crate::index_limits::{DEFAULT_MAX_SOURCE_FILE_BYTES, IndexLimits};

/// A source was rejected from metadata/object headers before allocation.
#[derive(Debug, thiserror::Error)]
#[error("source {path} is too large ({observed_bytes} bytes exceeds the {limit_bytes}-byte limit)")]
pub struct SourceTooLarge {
    pub path: String,
    pub observed_bytes: u64,
    pub limit_bytes: u64,
}

/// A source was skipped because it is binary, not text (nw-190).
///
/// Detected with the NUL-byte heuristic shared by ripgrep, git and grep: a file
/// is binary iff it contains a NUL byte in its leading window. Lossily decoding
/// such a file would mint garbage symbols, so it is skipped instead — but as a
/// typed, per-file error the caller can tolerate, never as a repo-fatal one.
#[derive(Debug, thiserror::Error)]
#[error("source {path} is binary (contains a NUL byte); skipped")]
pub struct BinarySource {
    pub path: String,
}

/// Maximum time to wait for a single `git cat-file --batch` response.
///
/// A hung-but-alive git process (e.g. a wedged pack read on a corrupt or
/// network-backed object store) would otherwise block the [`GitBareReader`]
/// `batch` Mutex — and every rayon parse thread queued behind it —
/// indefinitely, since the underlying `read_line`/`read_exact` calls have no
/// deadline. On timeout we kill the child so the reader thread unblocks, then
/// return `Err` so `read_file` falls back to a one-shot `git show` and the next
/// read re-spawns a fresh batch process.
const CAT_FILE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Abstracts how the indexer reads file contents and discovers files.
/// `FilesystemReader` preserves local behavior; `GitBareReader` (added in Task 6)
/// reads from blobless bare clones via a pooled, persistent `git cat-file --batch`
/// subprocess (one process per reader, reused for every file read).
pub trait ContentReader: Send + Sync {
    /// Read the full content of a file at `rel_path` (repo-relative).
    fn read_file(&self, rel_path: &Path) -> Result<String>;

    /// List the repo's files (repo-relative paths), skipping the shared skip-dirs.
    ///
    /// The two backends differ by design and callers should be aware:
    /// - `FilesystemReader` walks the **working tree** and applies `.gitignore` /
    ///   `.git/info/exclude` patterns, so untracked-ignored files are excluded.
    /// - `GitBareReader` lists the **committed tree** (`git ls-tree`), which
    ///   naturally omits untracked-ignored files but *retains* any file that is
    ///   tracked-but-ignored (committed then later gitignored). It does not run a
    ///   gitignore matcher — the tree membership is the filter.
    fn list_files(&self) -> Result<Vec<PathBuf>>;

    /// Return filesystem metadata for change detection.
    /// For FilesystemReader: `Some((mtime_secs, size_bytes))`.
    /// For GitBareReader: returns `None` (uses commit SHA instead of mtime).
    fn file_meta(&self, rel_path: &Path) -> Result<Option<(u64, u64)>>;

    /// The root path (for constructing absolute paths in parsers that need them).
    fn root(&self) -> &Path;

    /// An identifier for the content version (HEAD SHA for git, "local" for filesystem).
    fn version_id(&self) -> &str;

    /// Validated source-code input ceiling for this reader.
    fn max_source_file_bytes(&self) -> u64 {
        DEFAULT_MAX_SOURCE_FILE_BYTES
    }
}

/// Local filesystem reader — wraps the existing `ignore::WalkBuilder` + `fs::read_to_string`.
pub struct FilesystemReader {
    repo_path: PathBuf,
    limits: IndexLimits,
}

impl FilesystemReader {
    pub fn new(repo_path: &Path) -> Self {
        Self {
            repo_path: repo_path.to_path_buf(),
            limits: IndexLimits::default(),
        }
    }

    pub fn with_limits(repo_path: &Path, limits: IndexLimits) -> Self {
        Self {
            repo_path: repo_path.to_path_buf(),
            limits,
        }
    }
}

impl ContentReader for FilesystemReader {
    fn read_file(&self, rel_path: &Path) -> Result<String> {
        let abs = self.repo_path.join(rel_path);
        let observed_bytes = std::fs::metadata(&abs)
            .map_err(|e| anyhow::anyhow!("stat {}: {e}", abs.display()))?
            .len();
        if observed_bytes > self.limits.max_source_file_bytes() {
            return Err(SourceTooLarge {
                path: rel_path.display().to_string(),
                observed_bytes,
                limit_bytes: self.limits.max_source_file_bytes(),
            }
            .into());
        }
        // Bound the actual read as well as the metadata preflight so a file
        // that grows between stat and read cannot force an unbounded allocation.
        let file = std::fs::File::open(&abs)
            .map_err(|e| anyhow::anyhow!("open {}: {e}", abs.display()))?;
        let mut bounded = file.take(self.limits.max_source_file_bytes() + 1);
        // nw-190: read bytes and decode lossily rather than read_to_string, which
        // hard-errors on invalid UTF-8. Every call site propagates that error with
        // `?`, so ONE bad file aborted the whole repository index -- a reporter lost
        // ~350k symbols because exactly one of 2,429 non-UTF-8 tracked files had a
        // parseable source extension.
        //
        // from_utf8_lossy returns Cow::Borrowed with NO allocation when the input is
        // already valid UTF-8, so the overwhelmingly common path costs nothing; only
        // a file that genuinely contains invalid bytes allocates, and those bytes
        // become U+FFFD. A stray Latin-1 byte in a legacy JS comment therefore still
        // yields that file's symbols.
        let mut raw = Vec::new();
        bounded
            .read_to_end(&mut raw)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", abs.display()))?;
        // Genuinely binary content would decode into garbage symbols, so skip it
        // using the NUL-byte heuristic that ripgrep, git and grep all use: a file is
        // binary iff it contains a NUL, tested over the leading window.
        const BINARY_SNIFF_BYTES: usize = 8192;
        if raw[..raw.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
            return Err(BinarySource {
                path: rel_path.display().to_string(),
            }
            .into());
        }
        let source = String::from_utf8_lossy(&raw).into_owned();
        if source.len() as u64 > self.limits.max_source_file_bytes() {
            return Err(SourceTooLarge {
                path: rel_path.display().to_string(),
                observed_bytes: observed_bytes.max(source.len() as u64),
                limit_bytes: self.limits.max_source_file_bytes(),
            }
            .into());
        }
        Ok(source)
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

    fn max_source_file_bytes(&self) -> u64 {
        self.limits.max_source_file_bytes()
    }
}

/// One object resolved from the `git cat-file --batch` stream.
enum BatchObject {
    /// Object found — its full content as raw bytes.
    Found(Vec<u8>),
    /// Git reported `<spec> missing` — no such object/path at this revision.
    Missing,
    /// The object's git-reported size exceeded the reader's source limit. Its bytes
    /// were read-and-discarded (never materialized) to keep the stream framed;
    /// the caller skips the file, mirroring the filesystem oversized-file guard.
    TooLarge(u64),
}

/// Read and discard exactly `n` bytes from `r` using a small fixed buffer, so an
/// oversized object is never materialized in memory. Keeps the `cat-file --batch`
/// stream framed after a skipped blob. Pure (testable against any `Read`).
fn discard_exact<R: Read>(r: &mut R, mut n: usize) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    while n > 0 {
        let want = n.min(buf.len());
        r.read_exact(&mut buf[..want])?;
        n -= want;
    }
    Ok(())
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
    /// Parsed responses produced by the dedicated reader thread, in request
    /// order. `request` writes one spec to `stdin`, then `recv_timeout`s here so
    /// a wedged git read can never block the caller past [`CAT_FILE_READ_TIMEOUT`].
    responses: mpsc::Receiver<Result<BatchObject>>,
    /// Handle to the reader thread. Joined on drop (after the child is killed,
    /// which closes its stdout and unblocks the thread) so no thread leaks.
    reader: Option<JoinHandle<()>>,
}

impl CatFileBatch {
    /// Spawn `git -C <bare_path> cat-file --batch` with piped stdin/stdout, plus
    /// a dedicated reader thread that owns stdout and parses framed responses.
    fn spawn(bare_path: &Path, max_source_file_bytes: u64) -> Result<Self> {
        // The pooled batch process is long-lived with an interactive stdin/stdout
        // protocol, so it can't go through `run_git_with_timeout` (which nulls
        // stdin and drains stdout to EOF). Its read deadline is enforced per
        // request via `CAT_FILE_READ_TIMEOUT`. Still isolate it from the host's
        // system/global git config and credential helpers, like every other call.
        let mut cmd = Command::new("git");
        cmd.args([
            "-C",
            &bare_path.display().to_string(),
            "cat-file",
            "--batch",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        apply_git_isolation(&mut cmd);
        let mut child = cmd
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

        // The reader thread owns stdout so the blocking `read_line`/`read_exact`
        // calls happen off the request path; `request` only writes stdin and
        // waits on the channel with a deadline.
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("cat-file-batch-reader".to_string())
            .spawn(move || read_loop(BufReader::new(stdout), tx, max_source_file_bytes))
            .context("failed to spawn cat-file --batch reader thread")?;

        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            responses: rx,
            reader: Some(reader),
        })
    }

    /// Resolve one object by its `<sha>:<path>` spec.
    ///
    /// Returns `Ok(BatchObject::Missing)` when git reports the path missing,
    /// `Ok(BatchObject::TooLarge)` when the blob exceeds the size cap, and
    /// `Err` for I/O failures or a read timeout (the batch process has died or
    /// hung), so the caller can fall back to a one-shot `git show`.
    fn request(&mut self, sha: &str, rel_path: &Path) -> Result<BatchObject> {
        // `cat-file --batch` is line-delimited: a path containing a newline (git
        // permits it) would split into two request lines, so git emits two framed
        // responses and every subsequent read on this pooled process returns the
        // prior read's content (permanent desync). Refuse such a path — the file
        // is skipped (Err → caller's skip branch) but the stream stays framed.
        let spec = format!("{}:{}", sha, rel_path.display());
        if spec.contains('\n') || spec.contains('\r') {
            anyhow::bail!(
                "skipping path with embedded newline (unsupported by cat-file --batch): {}",
                rel_path.display()
            );
        }
        // Send the request line: "<sha>:<path>\n".
        writeln!(self.stdin, "{spec}").context("write request to cat-file --batch")?;
        self.stdin.flush().context("flush cat-file --batch stdin")?;

        // Wait for the reader thread's parsed response, but never longer than
        // CAT_FILE_READ_TIMEOUT — a hung-but-alive git must not wedge the Mutex.
        match self.responses.recv_timeout(CAT_FILE_READ_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Kill the child so its stdout closes and the reader thread
                // unblocks and exits (joined on drop). Surface an error so the
                // caller discards this batch and falls back to `git show`.
                let _ = self.child.kill();
                anyhow::bail!(
                    "cat-file --batch read timed out after {}s",
                    CAT_FILE_READ_TIMEOUT.as_secs()
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("cat-file --batch reader thread exited unexpectedly")
            }
        }
    }
}

/// Reader-thread loop: parse framed `cat-file --batch` responses from `stdout`
/// and forward each to `tx` in request order. Stops on the first parse/I/O error
/// (the stream is then desynced or closed) or once the receiver is dropped.
fn read_loop(
    mut stdout: BufReader<ChildStdout>,
    tx: mpsc::Sender<Result<BatchObject>>,
    max_source_file_bytes: u64,
) {
    loop {
        let result = read_one(&mut stdout, max_source_file_bytes);
        let is_err = result.is_err();
        if tx.send(result).is_err() {
            // Receiver gone (the batch was discarded) — nothing left to do.
            break;
        }
        if is_err {
            // The stream is broken or closed; further reads are meaningless.
            break;
        }
    }
}

/// Parse exactly one framed response: `<oid> <type> <size>\n` then `<size>`
/// content bytes and a trailing newline, or `<spec> missing\n`. Blobs over
/// `max_source_file_bytes` are read-and-discarded (not allocated) and reported
/// as [`BatchObject::TooLarge`].
fn read_one(
    stdout: &mut BufReader<ChildStdout>,
    max_source_file_bytes: u64,
) -> Result<BatchObject> {
    // Read the header: "<oid> <type> <size>\n" or "<spec> missing\n".
    let mut header = String::new();
    let n = stdout
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

    // Enforce the file-size ceiling. A bare clone has no filesystem size for the
    // scan-phase guard to check (file_meta returns None), so without this an
    // accident- or attacker-sized blob would be allocated whole via vec![0u8;
    // size]. Discard exactly `size` bytes plus the trailing newline to keep the
    // stream framed, but never materialize the blob.
    if size as u64 > max_source_file_bytes {
        discard_exact(stdout, size + 1).context("discard oversized cat-file --batch object")?;
        return Ok(BatchObject::TooLarge(size as u64));
    }

    // Read exactly `size` bytes of content, then consume the trailing newline.
    let mut content = vec![0u8; size];
    stdout
        .read_exact(&mut content)
        .context("read cat-file --batch object content")?;
    let mut newline = [0u8; 1];
    stdout
        .read_exact(&mut newline)
        .context("read cat-file --batch trailing newline")?;

    Ok(BatchObject::Found(content))
}

impl Drop for CatFileBatch {
    fn drop(&mut self) {
        // Kill and reap the child so no zombie git process leaks. Killing it
        // closes the child's stdout, which unblocks the reader thread's pending
        // read so it can exit.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
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
    limits: IndexLimits,
    /// Lazily-spawned pooled `cat-file --batch` process. `None` until the first
    /// read; reset to `None` if the process dies so the next read re-spawns.
    batch: Mutex<Option<CatFileBatch>>,
}

impl GitBareReader {
    pub fn new(bare_path: &Path, sha: &str) -> Self {
        Self {
            bare_path: bare_path.to_path_buf(),
            sha: sha.to_string(),
            limits: IndexLimits::default(),
            batch: Mutex::new(None),
        }
    }

    pub fn with_limits(bare_path: &Path, sha: &str, limits: IndexLimits) -> Self {
        Self {
            bare_path: bare_path.to_path_buf(),
            sha: sha.to_string(),
            limits,
            batch: Mutex::new(None),
        }
    }

    /// Resolve HEAD of the bare repo to a full SHA.
    pub fn from_head(bare_path: &Path) -> Result<Self> {
        Self::from_head_with_limits(bare_path, IndexLimits::default())
    }

    /// Resolve HEAD while applying the configured source-file limit.
    pub fn from_head_with_limits(bare_path: &Path, limits: IndexLimits) -> Result<Self> {
        let mut cmd = Command::new("git");
        cmd.args(["-C", &bare_path.display().to_string(), "rev-parse", "HEAD"]);
        let output = run_git_with_timeout(cmd, git_net_timeout())
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
        Ok(Self::with_limits(bare_path, &sha, limits))
    }

    /// One-shot fallback read used when the pooled `cat-file --batch` process is
    /// unavailable (failed to spawn, or died mid-stream).
    fn read_file_via_show(&self, rel_path: &Path) -> Result<String> {
        let spec = format!("{}:{}", self.sha, rel_path.display());
        // Enforce the size cap BEFORE transferring content, mirroring the batch
        // path's `TooLarge` skip. `git cat-file -s` reports the blob size without
        // emitting its bytes, so an oversized blob is never materialized here (the
        // batch reader's cap would otherwise be bypassed whenever this fallback
        // fires — spawn failure, mid-stream death, or timeout).
        let mut size_cmd = Command::new("git");
        size_cmd.args([
            "-C",
            &self.bare_path.display().to_string(),
            "cat-file",
            "-s",
            &spec,
        ]);
        let size_output = run_git_with_timeout(size_cmd, git_net_timeout())
            .with_context(|| format!("failed to preflight object size for {spec}"))?;
        if !size_output.status.success() {
            anyhow::bail!(
                "git cat-file -s {} failed: {}",
                spec,
                String::from_utf8_lossy(&size_output.stderr).trim()
            );
        }
        let size = String::from_utf8_lossy(&size_output.stdout)
            .trim()
            .parse::<u64>()
            .with_context(|| format!("invalid git object size for {spec}"))?;
        if size > self.limits.max_source_file_bytes() {
            tracing::warn!(
                "skipping oversized blob {} (exceeds {} bytes) via git show",
                rel_path.display(),
                self.limits.max_source_file_bytes()
            );
            return Err(SourceTooLarge {
                path: rel_path.display().to_string(),
                observed_bytes: size,
                limit_bytes: self.limits.max_source_file_bytes(),
            }
            .into());
        }
        let mut cmd = Command::new("git");
        cmd.args(["-C", &self.bare_path.display().to_string(), "show", &spec]);
        let output = run_git_with_timeout(cmd, git_net_timeout())
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
            match CatFileBatch::spawn(&self.bare_path, self.limits.max_source_file_bytes()) {
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
            Ok(BatchObject::TooLarge(observed_bytes)) => {
                // Mirror the filesystem oversized-file skip: the blob was already
                // read-and-discarded (never materialized) so the stream stays
                // framed. Do NOT fall back to `git show` — that would re-read the
                // oversized blob whole. Return Err so callers skip this one file
                // (their existing read_file Err branch) without failing the index.
                tracing::warn!(
                    "skipping oversized blob {} (exceeds {} bytes)",
                    rel_path.display(),
                    self.limits.max_source_file_bytes()
                );
                Err(SourceTooLarge {
                    path: rel_path.display().to_string(),
                    observed_bytes,
                    limit_bytes: self.limits.max_source_file_bytes(),
                }
                .into())
            }
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
        // `-z` yields NUL-terminated records with paths emitted verbatim (git's
        // `core.quotePath` C-quoting is disabled for `-z`), so non-ASCII paths
        // (`café.md`, CJK names) survive intact instead of arriving as
        // `"caf\303\251.md"` — which would never match a later cat-file spec and
        // silently drop the file. Dropping `--name-only` keeps the mode field so
        // we can skip symlink (120000) and gitlink/submodule (160000) entries,
        // mirroring `FilesystemReader`'s `follow_links(false)`; otherwise a
        // symlink's target-path text would be indexed as file content.
        let mut cmd = Command::new("git");
        cmd.args([
            "-C",
            &self.bare_path.display().to_string(),
            "ls-tree",
            "-r",
            "-z",
            &self.sha,
        ]);
        let output =
            run_git_with_timeout(cmd, git_net_timeout()).context("failed to run git ls-tree")?;
        if !output.status.success() {
            anyhow::bail!(
                "git ls-tree failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // Each NUL-terminated record is `<mode> <type> <object>\t<path>`.
        let mut files = Vec::new();
        for record in output.stdout.split(|&b| b == 0) {
            if record.is_empty() {
                continue;
            }
            let Some(tab) = record.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let meta = &record[..tab];
            let path_bytes = &record[tab + 1..];
            // Mode is the first space-delimited field of the metadata.
            let mode = meta.split(|&b| b == b' ').next().unwrap_or(&[]);
            if mode == b"120000" || mode == b"160000" {
                // Skip symlinks and gitlinks/submodules — neither is file content.
                continue;
            }
            let path = PathBuf::from(String::from_utf8_lossy(path_bytes).into_owned());
            if crate::index::path_in_skip_dir(&path) {
                continue;
            }
            files.push(path);
        }
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

    fn max_source_file_bytes(&self) -> u64 {
        self.limits.max_source_file_bytes()
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
    fn filesystem_reader_enforces_configured_source_limit_boundaries() {
        let dir = TempDir::new().unwrap();
        let limit = crate::index_limits::MIN_MAX_SOURCE_FILE_BYTES;
        std::fs::write(dir.path().join("below.rs"), vec![b'x'; limit as usize - 1]).unwrap();
        std::fs::write(dir.path().join("at.rs"), vec![b'x'; limit as usize]).unwrap();
        std::fs::write(dir.path().join("above.rs"), vec![b'x'; limit as usize + 1]).unwrap();
        let reader = FilesystemReader::with_limits(dir.path(), IndexLimits::new(limit).unwrap());
        assert!(reader.read_file(Path::new("below.rs")).is_ok());
        assert!(reader.read_file(Path::new("at.rs")).is_ok());
        let error = reader.read_file(Path::new("above.rs")).unwrap_err();
        let oversized = error.downcast_ref::<SourceTooLarge>().unwrap();
        assert_eq!(oversized.observed_bytes, limit + 1);
        assert_eq!(oversized.limit_bytes, limit);
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
    fn git_bare_reader_rejects_embedded_newline_path_without_wedging() {
        // A path with an embedded newline would split the `cat-file --batch`
        // request line and permanently desync the pooled reader. It must be
        // refused (Err) while the stream stays framed for subsequent reads.
        let (_tmp, bare, sha) = setup_bare_repo(&[("present.txt", "here")]);
        let reader = GitBareReader::new(&bare, &sha);

        assert_eq!(reader.read_file(Path::new("present.txt")).unwrap(), "here");
        assert!(
            reader.read_file(Path::new("weird\nname.txt")).is_err(),
            "embedded-newline path must be refused"
        );
        // The next valid read still succeeds — the stream was not desynced.
        assert_eq!(reader.read_file(Path::new("present.txt")).unwrap(), "here");
    }

    #[test]
    fn git_bare_reader_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GitBareReader>();
    }

    // ---------- discard_exact (pure helper) ----------

    #[test]
    fn discard_exact_consumes_exact_bytes() {
        use std::io::Cursor;
        let data = b"0123456789ABCDEF";
        let mut cur = Cursor::new(&data[..]);
        discard_exact(&mut cur, 10).unwrap();
        // The cursor is positioned exactly past the discarded bytes.
        let mut rest = Vec::new();
        cur.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"ABCDEF");
    }

    #[test]
    fn discard_exact_spans_internal_buffer() {
        use std::io::Cursor;
        // Larger than discard_exact's 8 KiB scratch buffer to exercise looping.
        let data = vec![7u8; 20_000];
        let mut cur = Cursor::new(data);
        discard_exact(&mut cur, 19_999).unwrap();
        let mut rest = Vec::new();
        cur.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, vec![7u8]);
    }

    #[test]
    fn discard_exact_errors_on_short_stream() {
        use std::io::Cursor;
        let mut cur = Cursor::new(vec![0u8; 5]);
        assert!(discard_exact(&mut cur, 10).is_err());
    }

    // ---------- FIX 1: pooled child death / git show fallback ----------

    /// Kill (and reap) the pooled `cat-file --batch` child out from under the
    /// reader, simulating a dead process, then confirm reads recover.
    #[test]
    fn git_bare_reader_recovers_when_pooled_child_dies() {
        let (_tmp, bare, sha) =
            setup_bare_repo(&[("a.txt", "alpha"), ("b.txt", "bravo"), ("c.txt", "charlie")]);
        let reader = GitBareReader::new(&bare, &sha);

        // First read spawns and uses the pooled batch process.
        assert_eq!(reader.read_file(Path::new("a.txt")).unwrap(), "alpha");
        assert!(
            reader.batch.lock().unwrap().is_some(),
            "batch process should be spawned after first read"
        );

        // Kill the pooled child to simulate a dead/hung-then-killed git.
        {
            let mut guard = reader.batch.lock().unwrap();
            let batch = guard.as_mut().expect("batch spawned above");
            batch.child.kill().unwrap();
            batch.child.wait().unwrap();
        }

        // The next read must still return correct content via the git show
        // fallback, and clear the dead batch so the following read re-spawns.
        assert_eq!(reader.read_file(Path::new("b.txt")).unwrap(), "bravo");
        assert!(
            reader.batch.lock().unwrap().is_none(),
            "dead batch should be discarded so the next read re-spawns"
        );

        // A subsequent read re-spawns through the batch path and works.
        assert_eq!(reader.read_file(Path::new("c.txt")).unwrap(), "charlie");
        assert!(
            reader.batch.lock().unwrap().is_some(),
            "batch process should be re-spawned after a fallback read"
        );
        // ...and the re-spawned stream stays in sync across further reads.
        assert_eq!(reader.read_file(Path::new("a.txt")).unwrap(), "alpha");
    }

    /// Directly exercise the one-shot `git show` fallback path.
    #[test]
    fn git_bare_reader_read_file_via_show() {
        let (_tmp, bare, sha) =
            setup_bare_repo(&[("hello.txt", "world"), ("dir/nested.txt", "deep")]);
        let reader = GitBareReader::new(&bare, &sha);

        assert_eq!(
            reader.read_file_via_show(Path::new("hello.txt")).unwrap(),
            "world"
        );
        assert_eq!(
            reader
                .read_file_via_show(Path::new("dir/nested.txt"))
                .unwrap(),
            "deep"
        );
        // A missing path must error rather than return empty content.
        assert!(reader.read_file_via_show(Path::new("absent.txt")).is_err());
    }

    // ---------- T6.1: non-ASCII paths, symlinks, gitlinks ----------

    /// Files with accented / CJK names must be listed with their real paths and
    /// be readable — not returned as git's C-quoted `"caf\303\251.md"` form,
    /// which never matches a cat-file spec and silently drops the file.
    #[test]
    fn git_bare_reader_lists_non_ascii_paths() {
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("café.md", "accented content"),
            ("日本語.md", "cjk content"),
            ("plain.md", "ascii"),
        ]);
        let reader = GitBareReader::new(&bare, &sha);
        let files = reader.list_files().unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"café.md".to_string()),
            "accented path missing/quoted: {names:?}"
        );
        assert!(
            names.contains(&"日本語.md".to_string()),
            "CJK path missing/quoted: {names:?}"
        );
        // The real (unquoted) path must be readable via cat-file.
        assert_eq!(
            reader.read_file(Path::new("café.md")).unwrap(),
            "accented content"
        );
        assert_eq!(
            reader.read_file(Path::new("日本語.md")).unwrap(),
            "cjk content"
        );
    }

    /// Symlink (mode 120000) and gitlink/submodule (mode 160000) tree entries
    /// must be skipped by `list_files` — a symlink's target-path text is not file
    /// content, and a gitlink has no blob to read. Mirrors `FilesystemReader`'s
    /// `follow_links(false)`.
    #[cfg(unix)]
    #[test]
    fn git_bare_reader_skips_symlinks_and_gitlinks() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src_repo");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&src)
                .output()
                .unwrap();
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);

        std::fs::write(src.join("real.txt"), "real content").unwrap();
        symlink("real.txt", src.join("link.txt")).unwrap();
        git(&["add", "real.txt", "link.txt"]);
        git(&["commit", "-m", "init"]);

        // Register a gitlink (mode 160000) pointing at the commit we just made.
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&src)
            .output()
            .unwrap();
        let head = String::from_utf8(head.stdout).unwrap().trim().to_string();
        git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},submod"),
        ]);
        git(&["commit", "-m", "add gitlink"]);

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
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&bare)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let reader = GitBareReader::new(&bare, &sha);
        let names: Vec<String> = reader
            .list_files()
            .unwrap()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"real.txt".to_string()),
            "regular file must still be listed: {names:?}"
        );
        assert!(
            !names.contains(&"link.txt".to_string()),
            "symlink must be skipped: {names:?}"
        );
        assert!(
            !names.contains(&"submod".to_string()),
            "gitlink/submodule must be skipped: {names:?}"
        );
    }

    // ---------- FIX 2: oversized blob cap on bare clones ----------

    /// A blob whose git-reported size exceeds the configured source limit must be skipped
    /// without being materialized, and the batch stream must stay framed so later
    /// valid reads through the same reader still succeed.
    #[test]
    fn git_bare_reader_skips_oversized_blob_and_keeps_stream_usable() {
        // Just over the cap; setup_bare_repo borrows &str so build it first.
        let limit = DEFAULT_MAX_SOURCE_FILE_BYTES;
        let big = "x".repeat(limit as usize + 1_000);
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("small.txt", "tiny"),
            ("big.txt", big.as_str()),
            ("after.txt", "still here"),
        ]);
        let reader = GitBareReader::new(&bare, &sha);

        // A normal read first, to prime the pooled batch process.
        assert_eq!(reader.read_file(Path::new("small.txt")).unwrap(), "tiny");

        // The oversized blob is skipped via a clear error (read_file's Err branch
        // makes callers skip just this file, not fail the whole index).
        let err = reader.read_file(Path::new("big.txt")).unwrap_err();
        assert!(
            err.to_string().contains("too large"),
            "expected an oversized-skip error, got: {err}"
        );
        // The batch must NOT have been discarded — the stream was kept framed by
        // read-and-discard, so it is still the live pooled process.
        assert!(
            reader.batch.lock().unwrap().is_some(),
            "oversized skip must not kill the pooled batch process"
        );

        // Later valid reads through the SAME reader still succeed, proving the
        // stream stayed in sync after discarding the oversized object.
        assert_eq!(
            reader.read_file(Path::new("after.txt")).unwrap(),
            "still here"
        );
        assert_eq!(reader.read_file(Path::new("small.txt")).unwrap(), "tiny");
    }

    #[test]
    fn git_show_fallback_enforces_the_same_object_size_limit() {
        let limit = crate::index_limits::MIN_MAX_SOURCE_FILE_BYTES;
        let big = "x".repeat(limit as usize + 1);
        let (_tmp, bare, sha) = setup_bare_repo(&[("big.rs", big.as_str())]);
        let reader = GitBareReader::with_limits(
            &bare,
            &sha,
            IndexLimits::new(limit).expect("test limit is valid"),
        );
        let error = reader.read_file_via_show(Path::new("big.rs")).unwrap_err();
        let oversized = error.downcast_ref::<SourceTooLarge>().unwrap();
        assert_eq!(oversized.observed_bytes, limit + 1);
        assert_eq!(oversized.limit_bytes, limit);
    }
}
#[cfg(test)]
mod non_utf8_tests {
    use super::*;
    use std::io::Write;

    /// nw-190: invalid UTF-8 must not abort the read. A reporter lost ~350k
    /// symbols because exactly one of 2,429 non-UTF-8 tracked files had a
    /// parseable source extension and read_to_string hard-errored.
    #[test]
    fn invalid_utf8_decodes_lossily_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.js");
        // Valid JS with a stray Latin-1 byte in a comment, as in older sources.
        let mut bytes = b"// copyright \xA9 2004\nfunction cleanPaste() { return 1; }\n".to_vec();
        bytes.push(0xFF);
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();

        let reader = FilesystemReader::new(dir.path());
        let source = reader
            .read_file(Path::new("legacy.js"))
            .expect("invalid UTF-8 must not fail the read");
        assert!(
            source.contains("function cleanPaste()"),
            "the file's real content must survive: {source:?}"
        );
        assert!(
            source.contains('\u{FFFD}'),
            "invalid bytes should become the replacement character"
        );
    }

    /// Binary content would mint garbage symbols, so it is refused -- but with a
    /// typed error the caller skips, never a repo-fatal one.
    #[test]
    fn binary_content_is_refused_with_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.js");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&[0x4d, 0x5a, 0x00, 0x01, 0x02, 0x03])
            .unwrap();

        let reader = FilesystemReader::new(dir.path());
        let error = reader
            .read_file(Path::new("blob.js"))
            .expect_err("binary must be refused");
        assert!(
            error.downcast_ref::<BinarySource>().is_some(),
            "must be a typed BinarySource so the caller can skip it: {error}"
        );
    }

    /// Valid UTF-8 is unchanged and takes the zero-allocation Cow path.
    #[test]
    fn valid_utf8_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.js"), "const a = '\u{1F600}';\n").unwrap();
        let reader = FilesystemReader::new(dir.path());
        let source = reader.read_file(Path::new("ok.js")).unwrap();
        assert_eq!(source, "const a = '\u{1F600}';\n");
    }
}
