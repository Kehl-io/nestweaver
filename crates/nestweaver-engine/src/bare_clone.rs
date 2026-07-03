//! Bare clone management for server-mode indexing.
//!
//! `BareClone` wraps a single blobless bare clone with fetch, HEAD resolution,
//! and diff operations. `BareCloneWorkspace` manages multiple bare clones in
//! a workspace directory with clone, fetch, remove, and list operations.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// A single blobless bare clone of a remote git repository.
#[derive(Debug, Clone)]
pub struct BareClone {
    /// Path to the bare clone directory (e.g. `/data/workspace/repo.git`).
    pub path: PathBuf,
    /// The remote URL this clone tracks.
    pub url: String,
}

impl BareClone {
    /// Check whether the given path looks like a valid bare git repository
    /// (has a `HEAD` file).
    pub fn is_valid_at(path: &Path) -> bool {
        path.join("HEAD").exists()
    }

    /// Fetch the latest commits and trees from origin (no blobs unless demanded).
    pub fn fetch(&self) -> Result<()> {
        self.fetch_branch(None)
    }

    /// Fetch a specific branch from origin, or all refs if `branch` is `None`.
    ///
    /// For branch-specific fetches, uses an explicit refspec
    /// (`<branch>:refs/heads/<branch>`) so the local ref is updated in the
    /// bare repo. Without this, `git fetch origin <branch>` in a bare clone
    /// only updates FETCH_HEAD, and `rev-parse origin/<branch>` fails.
    pub fn fetch_branch(&self, branch: Option<&str>) -> Result<()> {
        // SSRF guard: validate + resolve the remote immediately before fetching,
        // pinning the connect IP for http(s) (DNS-rebinding defense). file://
        // passes through with no args.
        let guard = crate::ssrf::guard_git_url(&self.url)?;
        let mut cmd = Command::new("git");
        cmd.args(&guard.config_args);
        cmd.arg("-C").arg(&self.path).args(["fetch", "origin"]);
        if let Some(b) = branch {
            cmd.arg(format!("{b}:refs/heads/{b}"));
        }
        // Hard timeout so a blackholed remote can't wedge the worker (and its
        // semaphore permit) forever — it kills+reaps the child on timeout.
        let output = crate::git_cmd::run_git_with_timeout(cmd, crate::git_cmd::GIT_TIMEOUT)
            .context("failed to run git fetch")?;
        if !output.status.success() {
            anyhow::bail!(
                "git fetch failed for {}: {}",
                self.url,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Resolve the SHA for an arbitrary ref (e.g. `origin/develop`).
    pub fn sha_for_ref(&self, reference: &str) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.path).args(["rev-parse", reference]);
        let output = crate::git_cmd::run_git_with_timeout(cmd, crate::git_cmd::GIT_TIMEOUT)
            .with_context(|| format!("failed to run git rev-parse {reference}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "git rev-parse {} failed: {}",
                reference,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Get the SHA of HEAD (the default branch tip).
    pub fn head_sha(&self) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.path).args(["rev-parse", "HEAD"]);
        let output = crate::git_cmd::run_git_with_timeout(cmd, crate::git_cmd::GIT_TIMEOUT)
            .context("failed to run git rev-parse HEAD")?;
        if !output.status.success() {
            anyhow::bail!(
                "git rev-parse HEAD failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Diff two SHAs for file-level changes.
    pub fn diff_name_status(
        &self,
        old_sha: &str,
        new_sha: &str,
    ) -> Result<Vec<crate::git_diff::FileChange>> {
        crate::git_diff::detect_changes(&self.path, old_sha, new_sha)
    }

    /// Check if `old_sha` is an ancestor of `new_sha`.
    pub fn is_ancestor(&self, old: &str, new: &str) -> Result<bool> {
        Ok(crate::git_diff::is_ancestor(&self.path, old, new))
    }

    /// Check remote HEAD via `git ls-remote` (lightweight, no object transfer).
    pub fn ls_remote_head(&self) -> Result<String> {
        // SSRF guard before contacting the remote (see `fetch_branch`).
        let guard = crate::ssrf::guard_git_url(&self.url)?;
        let mut cmd = Command::new("git");
        cmd.args(&guard.config_args);
        cmd.arg("-C")
            .arg(&self.path)
            .args(["ls-remote", "origin", "HEAD"]);
        let output = crate::git_cmd::run_git_with_timeout(cmd, crate::git_cmd::GIT_TIMEOUT)
            .context("failed to run git ls-remote")?;
        if !output.status.success() {
            anyhow::bail!(
                "git ls-remote failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let line = String::from_utf8_lossy(&output.stdout);
        let sha = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if sha.is_empty() {
            anyhow::bail!("ls-remote returned empty SHA for {}", self.url);
        }
        Ok(sha)
    }

    /// Verify the bare clone exists and has a valid HEAD.
    pub fn is_valid(&self) -> bool {
        Self::is_valid_at(&self.path)
    }
}

/// Manages all bare clones in a workspace directory.
///
/// Each clone lives at `<root>/<repo-name>.git`. The directory name is derived
/// from the URL via [`crate::pull::clone_dir_name_from_url`], which appends a
/// short URL hash so same-basename repos from different hosts/orgs don't collide.
pub struct BareCloneWorkspace {
    /// Root directory for all bare clones.
    pub root: PathBuf,
}

impl BareCloneWorkspace {
    /// Create a new workspace, ensuring the root directory exists.
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("failed to create workspace dir: {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Get or create a bare clone for a repo URL.
    ///
    /// If the clone already exists and is valid, returns it immediately.
    /// Otherwise creates a new blobless bare clone.
    pub fn ensure_clone(&self, url: &str) -> Result<BareClone> {
        let name = crate::pull::clone_dir_name_from_url(url);
        let dest = self.root.join(format!("{}.git", name));

        if dest.exists() && BareClone::is_valid_at(&dest) {
            // Defense-in-depth: even with the URL-hashed dir name, confirm the
            // stored origin matches the requested URL before reusing. A mismatch
            // means the directory holds a different repo's clone (stale or a hash
            // collision) — fall through to remove and re-clone.
            if read_origin_url(&dest).ok().as_deref() == Some(url) {
                return Ok(BareClone {
                    path: dest,
                    url: url.to_string(),
                });
            }
        }

        // SSRF guard: validate the URL and resolve+pin the remote IP BEFORE any
        // filesystem mutation or git spawn. Rejects internal targets and
        // un-pinnable schemes (git://) up front so no clone dir is created.
        // file:// passes through with no args.
        let guard = crate::ssrf::guard_git_url(url)?;

        // Remove any invalid remnant (or origin-mismatched clone) before cloning.
        if dest.exists() {
            std::fs::remove_dir_all(&dest).ok();
        }

        let mut cmd = Command::new("git");
        cmd.args(&guard.config_args);
        cmd.args([
            "clone",
            "--filter=blob:none",
            "--bare",
            "--",
            url,
            &dest.display().to_string(),
        ]);
        // Hard timeout so a hung clone against an unreachable remote can't wedge
        // the worker task and leak its semaphore permit.
        let output = crate::git_cmd::run_git_with_timeout(cmd, crate::git_cmd::GIT_TIMEOUT)
            .with_context(|| {
                format!("failed to run git clone --filter=blob:none --bare for {url}")
            })?;

        if !output.status.success() {
            anyhow::bail!(
                "git clone --bare failed for {}: {}",
                url,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(BareClone {
            path: dest,
            url: url.to_string(),
        })
    }

    /// Remove a bare clone for a repo URL.
    pub fn remove(&self, url: &str) -> Result<()> {
        let name = crate::pull::clone_dir_name_from_url(url);
        let dest = self.root.join(format!("{}.git", name));
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("failed to remove bare clone at {}", dest.display()))?;
        }
        Ok(())
    }

    /// List all existing bare clones in the workspace.
    ///
    /// Scans for `*.git` directories that have a valid `HEAD` file.
    pub fn list_clones(&self) -> Result<Vec<BareClone>> {
        let mut clones = Vec::new();
        let entries = std::fs::read_dir(&self.root)
            .with_context(|| format!("failed to read workspace dir: {}", self.root.display()))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.ends_with(".git")
                && BareClone::is_valid_at(&path)
            {
                // Try to read the origin URL from git config.
                let url = read_origin_url(&path).unwrap_or_default();
                clones.push(BareClone { path, url });
            }
        }

        Ok(clones)
    }

    /// Total disk usage of all bare clones in bytes.
    pub fn disk_usage(&self) -> Result<u64> {
        let mut total = 0u64;
        let entries = std::fs::read_dir(&self.root)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path)?;
            }
        }
        Ok(total)
    }
}

/// Read the origin remote URL from a bare repo's git config.
fn read_origin_url(bare_path: &Path) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(bare_path)
        .args(["config", "--get", "remote.origin.url"]);
    let output = crate::git_cmd::run_git_with_timeout(cmd, crate::git_cmd::GIT_TIMEOUT)
        .context("failed to run git config")?;
    if !output.status.success() {
        anyhow::bail!("no origin URL configured");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Recursively compute directory size in bytes.
fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if path.is_file() {
        return Ok(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p)?;
        } else {
            total += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Helper: create a source git repo with a single commit containing the given files.
    fn create_source_repo(dir: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();

        for (path, content) in files {
            let full = dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }

        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn bare_clone_workspace_ensure_clone() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "world")]);

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let clone = ws
            .ensure_clone(&format!("file://{}", src.display()))
            .unwrap();

        assert!(clone.is_valid());
        assert!(clone.path.exists());
        assert!(
            clone
                .path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(".git")
        );
    }

    #[test]
    fn bare_clone_workspace_ensure_clone_idempotent() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "world")]);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let clone1 = ws.ensure_clone(&url).unwrap();
        let clone2 = ws.ensure_clone(&url).unwrap();

        // Same path returned both times.
        assert_eq!(clone1.path, clone2.path);
    }

    #[test]
    fn bare_clone_workspace_distinct_dirs_for_same_basename() {
        let tmp = TempDir::new().unwrap();
        // Two different source repos that share the same basename ("api"),
        // mirroring github.com/acme/api vs gitlab.com/vendor/api.
        let src1 = tmp.path().join("acme").join("api");
        let src2 = tmp.path().join("vendor").join("api");
        create_source_repo(&src1, &[("a.txt", "from acme")]);
        create_source_repo(&src2, &[("b.txt", "from vendor")]);
        let url1 = format!("file://{}", src1.display());
        let url2 = format!("file://{}", src2.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let clone1 = ws.ensure_clone(&url1).unwrap();
        let clone2 = ws.ensure_clone(&url2).unwrap();

        // Distinct clone directories despite the shared basename.
        assert_ne!(
            clone1.path, clone2.path,
            "same-basename URLs must not share a clone dir"
        );
        assert!(clone1.is_valid());
        assert!(clone2.is_valid());

        // Each clone tracks its own origin.
        assert_eq!(read_origin_url(&clone1.path).unwrap(), url1);
        assert_eq!(read_origin_url(&clone2.path).unwrap(), url2);

        // Both clones coexist on disk.
        let clones = ws.list_clones().unwrap();
        assert_eq!(clones.len(), 2);
    }

    #[test]
    fn ensure_clone_reclones_on_origin_mismatch() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "world")]);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let clone = ws.ensure_clone(&url).unwrap();

        // Simulate a foreign/stale clone occupying the dir by rewriting origin.
        Command::new("git")
            .arg("-C")
            .arg(&clone.path)
            .args([
                "config",
                "remote.origin.url",
                "https://example.com/other.git",
            ])
            .output()
            .unwrap();
        assert_eq!(
            read_origin_url(&clone.path).unwrap(),
            "https://example.com/other.git"
        );

        // ensure_clone must detect the mismatch, drop the dir, and re-clone.
        let reclone = ws.ensure_clone(&url).unwrap();
        assert_eq!(reclone.path, clone.path);
        assert_eq!(read_origin_url(&reclone.path).unwrap(), url);
    }

    #[test]
    fn bare_clone_workspace_remove() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "world")]);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let clone = ws.ensure_clone(&url).unwrap();
        assert!(clone.path.exists());

        ws.remove(&url).unwrap();
        assert!(!clone.path.exists());
    }

    #[test]
    fn bare_clone_workspace_list_clones() {
        let tmp = TempDir::new().unwrap();
        let src1 = tmp.path().join("repo-a");
        let src2 = tmp.path().join("repo-b");
        create_source_repo(&src1, &[("a.txt", "a")]);
        create_source_repo(&src2, &[("b.txt", "b")]);

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        ws.ensure_clone(&format!("file://{}", src1.display()))
            .unwrap();
        ws.ensure_clone(&format!("file://{}", src2.display()))
            .unwrap();

        let clones = ws.list_clones().unwrap();
        assert_eq!(clones.len(), 2);
    }

    #[test]
    fn bare_clone_head_sha() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "world")]);

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let clone = ws
            .ensure_clone(&format!("file://{}", src.display()))
            .unwrap();

        let sha = clone.head_sha().unwrap();
        assert!(!sha.is_empty());
        assert_eq!(sha.len(), 40, "SHA should be 40 hex chars");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn bare_clone_fetch() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "world")]);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let clone = ws.ensure_clone(&url).unwrap();
        let sha_before = clone.head_sha().unwrap();

        // Add a new commit to the source repo.
        std::fs::write(src.join("new.txt"), "new content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&src)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "second"])
            .current_dir(&src)
            .output()
            .unwrap();

        // Fetch and verify HEAD changed.
        clone.fetch().unwrap();

        // After fetch, HEAD of the bare clone tracks the remote's default branch.
        // We need to check the remote tracking ref, not HEAD directly (HEAD in a
        // bare clone doesn't auto-advance on fetch -- it tracks the local default).
        // Use ls-remote to check the source has a new SHA.
        let src_sha = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&src)
            .output()
            .unwrap();
        let src_sha = String::from_utf8_lossy(&src_sha.stdout).trim().to_string();
        assert_ne!(
            sha_before, src_sha,
            "source should have a new SHA after the second commit"
        );
    }

    #[test]
    fn bare_clone_is_valid_at() {
        let tmp = TempDir::new().unwrap();
        assert!(!BareClone::is_valid_at(tmp.path()));

        // A valid bare repo has a HEAD file.
        std::fs::write(tmp.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(BareClone::is_valid_at(tmp.path()));
    }

    #[test]
    fn bare_clone_workspace_disk_usage() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "some content here")]);

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        ws.ensure_clone(&format!("file://{}", src.display()))
            .unwrap();

        let usage = ws.disk_usage().unwrap();
        assert!(usage > 0, "disk usage should be > 0");
    }

    #[test]
    fn ensure_clone_rejects_internal_and_unpinnable_schemes() {
        let tmp = TempDir::new().unwrap();
        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();

        // Internal IP literal and an un-pinnable git:// scheme must both be
        // rejected by the SSRF guard BEFORE git is spawned — no clone dir.
        for url in ["https://127.0.0.1/x", "git://github.com/x"] {
            let res = ws.ensure_clone(url);
            assert!(res.is_err(), "expected {url} to be rejected");

            let name = crate::pull::clone_dir_name_from_url(url);
            let dest = ws.root.join(format!("{}.git", name));
            assert!(
                !dest.exists(),
                "no clone dir should be created for rejected {url}"
            );
        }
    }

    #[test]
    fn fetch_branch_creates_resolvable_ref() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("hello.txt", "world")]);

        // Create a "develop" branch in the source repo with a new commit.
        Command::new("git")
            .args(["checkout", "-b", "develop"])
            .current_dir(&src)
            .output()
            .unwrap();
        std::fs::write(src.join("dev.txt"), "develop content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&src)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "develop commit"])
            .current_dir(&src)
            .output()
            .unwrap();

        // Get the develop SHA from the source.
        let dev_sha_output = Command::new("git")
            .args(["rev-parse", "develop"])
            .current_dir(&src)
            .output()
            .unwrap();
        let expected_sha = String::from_utf8_lossy(&dev_sha_output.stdout)
            .trim()
            .to_string();

        // Clone as bare (like the server does).
        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let bare = ws
            .ensure_clone(&format!("file://{}", src.display()))
            .unwrap();

        // Fetch the develop branch with explicit refspec.
        bare.fetch_branch(Some("develop")).unwrap();

        // Resolve via refs/heads/develop (what the worker does).
        let resolved = bare.sha_for_ref("refs/heads/develop").unwrap();
        assert_eq!(
            resolved, expected_sha,
            "refs/heads/develop should resolve to the develop SHA"
        );

        // Verify origin/develop does NOT exist in a bare clone
        // (this is the bug we fixed — origin/ refs aren't created).
        let origin_result = bare.sha_for_ref("origin/develop");
        assert!(
            origin_result.is_err(),
            "origin/develop should NOT resolve in a bare clone"
        );
    }
}
