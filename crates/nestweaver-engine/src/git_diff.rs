//! Git-based change detection for incremental indexing.
//!
//! Provides utilities for comparing two git SHAs to identify which files
//! were added, modified, deleted, or renamed between commits. Used by the
//! incremental re-indexing pipeline to avoid re-parsing every file on each run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

/// Represents a file-level change between two git commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
}

/// Parses the tab-delimited output of `git diff --name-status`.
///
/// Each line has one of the following formats:
/// - `A\t<path>` — file added
/// - `M\t<path>` — file modified
/// - `D\t<path>` — file deleted
/// - `R###\t<old_path>\t<new_path>` — file renamed (### is similarity score)
///
/// Lines with unknown status codes are skipped with a debug log.
fn parse_diff_output(output: &str) -> Vec<FileChange> {
    let mut changes = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.is_empty() {
            continue;
        }

        let status = parts[0];

        if status == "A" {
            if let Some(path) = parts.get(1) {
                changes.push(FileChange::Added(PathBuf::from(path)));
            }
        } else if status == "M" {
            if let Some(path) = parts.get(1) {
                changes.push(FileChange::Modified(PathBuf::from(path)));
            }
        } else if status == "D" {
            if let Some(path) = parts.get(1) {
                changes.push(FileChange::Deleted(PathBuf::from(path)));
            }
        } else if status.starts_with('R') {
            match (parts.get(1), parts.get(2)) {
                (Some(from), Some(to)) => {
                    changes.push(FileChange::Renamed {
                        from: PathBuf::from(from),
                        to: PathBuf::from(to),
                    });
                }
                _ => {
                    tracing::debug!(status, line, "rename entry missing paths, skipping");
                }
            }
        } else {
            tracing::debug!(status, line, "unknown git diff status code, skipping");
        }
    }

    changes
}

/// Detects file changes between two git commits in a repository.
///
/// Runs `git diff --name-status <old_sha> <new_sha>` in `repo_path` and
/// returns the parsed list of `FileChange` values.
pub fn detect_changes(
    repo_path: &Path,
    old_sha: &str,
    new_sha: &str,
) -> Result<Vec<FileChange>, anyhow::Error> {
    let output = Command::new("git")
        .arg("diff")
        .arg("--name-status")
        .arg(old_sha)
        .arg(new_sha)
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("failed to run git diff in {}", repo_path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git diff failed in {}: {}",
            repo_path.display(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_diff_output(&stdout))
}

/// Returns the SHA of the current HEAD commit in a git repository.
///
/// Runs `git rev-parse HEAD` in `repo_path` and returns the trimmed SHA string.
/// Returns an error if `repo_path` is not a git repository or git is not available.
pub fn current_head_sha(repo_path: &Path) -> Result<String, anyhow::Error> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo_path)
        .output()
        .with_context(|| {
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
    Command::new("git")
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(old_sha)
        .arg(new_sha)
        .current_dir(repo_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_output_handles_all_statuses() {
        let input = "A\tsrc/new_file.rs\n\
                     M\tsrc/modified.rs\n\
                     D\tsrc/deleted.rs\n\
                     R100\tsrc/old_name.rs\tsrc/new_name.rs\n";

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
        let input = "X\tunknown.txt\n";
        let changes = parse_diff_output(input);
        assert!(changes.is_empty());
    }

    #[test]
    fn parse_diff_output_handles_empty_input() {
        let changes = parse_diff_output("");
        assert!(changes.is_empty());
    }
}
