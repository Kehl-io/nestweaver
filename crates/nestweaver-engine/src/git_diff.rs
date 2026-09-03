//! Git-based change detection for incremental indexing.
//!
//! Provides utilities for comparing two git SHAs to identify which files
//! were added, modified, deleted, or renamed between commits. Used by the
//! incremental re-indexing pipeline to avoid re-parsing every file on each run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use crate::git_cmd::{git_net_timeout, run_git_with_timeout};

/// Resolve an untrusted revision string to a commit object ID.
///
/// The `--end-of-options` separator is security-sensitive: callers may accept
/// revision strings from external input, and Git otherwise interprets values
/// such as `--output=<path>` as command-line options.  Only the validated hex
/// object ID returned here should be passed to subsequent Git commands.
pub(crate) fn resolve_commit_ref(
    repo_path: &Path,
    revision: &str,
) -> Result<String, anyhow::Error> {
    let commit_revision = format!("{revision}^{{commit}}");
    let mut cmd = Command::new("git");
    cmd.arg("rev-parse")
        .arg("--verify")
        .arg("--quiet")
        .arg("--end-of-options")
        .arg(&commit_revision)
        .current_dir(repo_path);

    let output = run_git_with_timeout(cmd, git_net_timeout()).with_context(|| {
        format!(
            "failed to resolve git commit ref in {}",
            repo_path.display()
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "invalid git commit ref in {}: {}",
            repo_path.display(),
            stderr.trim()
        );
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("git rev-parse returned a non-UTF-8 object ID")?;
    let oid = stdout.strip_suffix('\n').unwrap_or(stdout);
    if oid.is_empty() || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("git rev-parse returned an invalid commit object ID");
    }

    Ok(oid.to_owned())
}

/// Represents a file-level change between two git commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
}

/// Convert a raw path token from `git diff -z` into a `PathBuf`. The bytes are
/// emitted verbatim (no `core.quotePath` C-quoting under `-z`), so non-ASCII
/// UTF-8 names like `café.md` survive intact.
fn bytes_to_pathbuf(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Parses the NUL-delimited output of `git diff --name-status -z`.
///
/// `-z` emits records as NUL-terminated tokens with paths quoted verbatim
/// (unlike the default, which C-quotes any path containing non-ASCII bytes and
/// would break incremental adds/deletes for accented/CJK filenames):
/// - `A\0<path>\0` — file added
/// - `M\0<path>\0` — file modified
/// - `D\0<path>\0` — file deleted
/// - `R###\0<old_path>\0<new_path>\0` — renamed (### is the similarity score)
/// - `C###\0<old_path>\0<new_path>\0` — copied (only with `--find-copies`)
///
/// Renames and copies consume two path tokens; every other change consumes one.
/// Unknown status codes are skipped with a debug log.
fn parse_diff_output(output: &[u8]) -> Vec<FileChange> {
    let mut changes = Vec::new();

    // Split on NUL; drop the trailing empty token after the final terminator.
    let tokens: Vec<&[u8]> = output
        .split(|&b| b == 0)
        .filter(|t| !t.is_empty())
        .collect();

    let mut i = 0;
    while i < tokens.len() {
        let status = String::from_utf8_lossy(tokens[i]);
        i += 1;
        match status.chars().next().unwrap_or(' ') {
            'A' => {
                if let Some(path) = tokens.get(i) {
                    changes.push(FileChange::Added(bytes_to_pathbuf(path)));
                    i += 1;
                }
            }
            'M' => {
                if let Some(path) = tokens.get(i) {
                    changes.push(FileChange::Modified(bytes_to_pathbuf(path)));
                    i += 1;
                }
            }
            'D' => {
                if let Some(path) = tokens.get(i) {
                    changes.push(FileChange::Deleted(bytes_to_pathbuf(path)));
                    i += 1;
                }
            }
            // Renames and copies both carry two path tokens; consume both so the
            // token stream stays framed regardless.
            'R' | 'C' => match (tokens.get(i), tokens.get(i + 1)) {
                (Some(from), Some(to)) => {
                    changes.push(FileChange::Renamed {
                        from: bytes_to_pathbuf(from),
                        to: bytes_to_pathbuf(to),
                    });
                    i += 2;
                }
                _ => {
                    tracing::debug!(%status, "rename/copy entry missing paths, skipping");
                }
            },
            _ => {
                tracing::debug!(%status, "unknown git diff status code, skipping");
            }
        }
    }

    changes
}

/// Detects file changes between two git commits in a repository.
///
/// Resolves both revisions to commit OIDs, then runs
/// `git diff --name-status <old_oid> <new_oid>` in `repo_path` and returns the
/// parsed list of `FileChange` values. Resolving first is security-sensitive:
/// MCP `brain_diff.since_sha` is caller-controlled, so neither revision may
/// reach `git diff` while it can still be interpreted as an option.
pub fn detect_changes(
    repo_path: &Path,
    old_sha: &str,
    new_sha: &str,
) -> Result<Vec<FileChange>, anyhow::Error> {
    let old_oid = resolve_commit_ref(repo_path, old_sha).context("resolve old git commit")?;
    let new_oid = resolve_commit_ref(repo_path, new_sha).context("resolve new git commit")?;

    let mut cmd = Command::new("git");
    cmd.arg("diff")
        .arg("--name-status")
        .arg("-z")
        .arg(old_oid)
        .arg(new_oid)
        .arg("--")
        .current_dir(repo_path);
    let output = run_git_with_timeout(cmd, git_net_timeout())
        .with_context(|| format!("failed to run git diff in {}", repo_path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git diff failed in {}: {}",
            repo_path.display(),
            stderr.trim()
        );
    }

    Ok(parse_diff_output(&output.stdout))
}

/// Returns the SHA of the current HEAD commit in a git repository.
///
/// Runs `git rev-parse HEAD` in `repo_path` and returns the trimmed SHA string.
/// Returns an error if `repo_path` is not a git repository or git is not available.
pub fn current_head_sha(repo_path: &Path) -> Result<String, anyhow::Error> {
    let mut cmd = Command::new("git");
    cmd.arg("rev-parse").arg("HEAD").current_dir(repo_path);
    let output = run_git_with_timeout(cmd, git_net_timeout()).with_context(|| {
        format!(
            "failed to run git rev-parse HEAD in {}",
            repo_path.display()
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git rev-parse HEAD failed in {}: {}",
            repo_path.display(),
            stderr.trim()
        );
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(sha)
}

/// Returns `true` if `old_sha` is an ancestor of `new_sha` in the repository.
///
/// Runs `git merge-base --is-ancestor <old_sha> <new_sha>`. Returns `false`
/// on any error (not a git repo, unknown SHA, etc.) so callers can treat the
/// result as a non-fatal check.
pub fn is_ancestor(repo_path: &Path, old_sha: &str, new_sha: &str) -> bool {
    let mut cmd = Command::new("git");
    cmd.arg("merge-base")
        .arg("--is-ancestor")
        .arg(old_sha)
        .arg(new_sha)
        .current_dir(repo_path);
    run_git_with_timeout(cmd, git_net_timeout())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_output_handles_all_statuses() {
        // `-z` output: NUL-terminated tokens (status, then 1 or 2 paths).
        let input: &[u8] = b"A\0src/new_file.rs\0\
                             M\0src/modified.rs\0\
                             D\0src/deleted.rs\0\
                             R100\0src/old_name.rs\0src/new_name.rs\0";

        let changes = parse_diff_output(input);

        assert_eq!(changes.len(), 4);
        assert_eq!(
            changes[0],
            FileChange::Added(PathBuf::from("src/new_file.rs"))
        );
        assert_eq!(
            changes[1],
            FileChange::Modified(PathBuf::from("src/modified.rs"))
        );
        assert_eq!(
            changes[2],
            FileChange::Deleted(PathBuf::from("src/deleted.rs"))
        );
        assert_eq!(
            changes[3],
            FileChange::Renamed {
                from: PathBuf::from("src/old_name.rs"),
                to: PathBuf::from("src/new_name.rs"),
            }
        );
    }

    #[test]
    fn parse_diff_output_skips_unknown_status() {
        let input: &[u8] = b"X\0unknown.txt\0";
        let changes = parse_diff_output(input);
        assert!(changes.is_empty());
    }

    #[test]
    fn parse_diff_output_handles_empty_input() {
        let changes = parse_diff_output(b"");
        assert!(changes.is_empty());
    }

    /// Non-ASCII (accented / CJK) paths must round-trip through the real
    /// `git diff --name-status -z` invocation as their true names — not git's
    /// C-quoted `"caf\303\251.md"` form, which would make incremental
    /// adds/deletes for such files silently miss.
    #[test]
    fn detect_changes_handles_non_ascii_paths() {
        use std::process::Command;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);

        std::fs::write(repo.join("base.txt"), "base").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        let old = current_head_sha(repo).unwrap();

        std::fs::write(repo.join("café.md"), "accented").unwrap();
        std::fs::write(repo.join("日本語.md"), "cjk").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "add non-ascii"]);
        let new = current_head_sha(repo).unwrap();

        let changes = detect_changes(repo, &old, &new).unwrap();
        assert!(
            changes.contains(&FileChange::Added(PathBuf::from("café.md"))),
            "accented path missing/quoted: {changes:?}"
        );
        assert!(
            changes.contains(&FileChange::Added(PathBuf::from("日本語.md"))),
            "CJK path missing/quoted: {changes:?}"
        );
    }

    /// Both revisions cross the same `git diff` option boundary. Resolve each
    /// to a commit OID before invoking diff so a caller-controlled
    /// `brain_diff.since_sha` cannot create or truncate an output file.
    #[test]
    fn detect_changes_resolves_refs_and_rejects_option_injection_without_writing() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);

        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&["add", "base.txt"]);
        git(&["commit", "-q", "-m", "base"]);
        let old_oid = current_head_sha(repo).unwrap();

        std::fs::write(repo.join("added.txt"), "added\n").unwrap();
        git(&["add", "added.txt"]);
        git(&["commit", "-q", "-m", "tip"]);
        let new_oid = current_head_sha(repo).unwrap();
        git(&["branch", "base-branch", &old_oid]);
        git(&["branch", "tip-branch", &new_oid]);
        git(&["tag", "base-tag", &old_oid]);
        git(&["tag", "tip-tag", &new_oid]);

        // Counterweights: the security boundary still accepts the full range
        // of commit-ish inputs callers already use, for both endpoints.
        for (old, new) in [
            (old_oid.as_str(), new_oid.as_str()),
            ("HEAD~1", "HEAD"),
            ("base-branch", "tip-branch"),
            ("base-tag", "tip-tag"),
        ] {
            let changes = detect_changes(repo, old, new).unwrap();
            assert_eq!(
                changes,
                vec![FileChange::Added(PathBuf::from("added.txt"))],
                "valid revisions {old:?}..{new:?} must retain diff behavior"
            );
        }

        let sentinel = repo.join("sentinel.bin");
        let sentinel_contents = b"do not truncate\0or overwrite\n";
        std::fs::write(&sentinel, sentinel_contents).unwrap();
        let malicious_old = format!("--output={}", sentinel.display());
        assert!(
            detect_changes(repo, &malicious_old, "HEAD").is_err(),
            "an option-like old revision must be rejected"
        );
        assert_eq!(
            std::fs::read(&sentinel).unwrap().as_slice(),
            sentinel_contents,
            "a rejected old revision must not truncate or overwrite its target"
        );

        let nonexistent = repo.join("must-not-be-created.out");
        let malicious_new = format!("--output={}", nonexistent.display());
        assert!(
            detect_changes(repo, "HEAD~1", &malicious_new).is_err(),
            "an option-like new revision must be rejected"
        );
        assert!(
            !nonexistent.exists(),
            "a rejected new revision must not create its output target"
        );
    }
}
