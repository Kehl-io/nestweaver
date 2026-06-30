//! Diff-based impact analysis for CI pipelines.
//!
//! Given a git revision range (e.g., `origin/main..HEAD`), extracts changed
//! files, parses old and new versions via tree-sitter, computes atomic changes,
//! and optionally sends them to a NestWeaver server for cross-repo impact
//! analysis.

use std::path::Path;
use std::process::Command;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::atomic_changes::{AtomicChange, ImpactResult, ImpactSeverity, compute_file_changes};

/// Configuration for diff-based impact analysis.
pub struct DiffImpactConfig<'a> {
    /// Git revision range, e.g. `origin/main..HEAD`
    pub diff_spec: &'a str,
    /// Path to the repository root
    pub repo_path: &'a Path,
    /// Repository URL (for canonical ID computation)
    pub repo_url: &'a str,
    /// Minimum severity to include in results
    pub min_severity: ImpactSeverity,
}

/// Result of diff-based impact analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffImpactResult {
    pub changes: Vec<AtomicChange>,
    pub impacts: Vec<ImpactResult>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Compute atomic changes from a git diff range.
///
/// Runs `git diff --name-only --diff-filter=ACMR <diff-spec>` to find changed
/// files, then for each file parses old (base) and new (HEAD) versions to
/// produce atomic changes.
pub fn compute_diff_changes(
    repo_path: &Path,
    diff_spec: &str,
    repo_url: &str,
) -> Result<Vec<AtomicChange>, anyhow::Error> {
    // Parse the base ref from the diff spec (everything before `..`)
    let base_ref = diff_spec
        .split("..")
        .next()
        .context("invalid diff spec: expected format 'base..head'")?;

    // Use --name-status with -M to detect renames (status R). For renamed
    // files, git only reports the new path with --name-only, so
    // `git show base_ref:<new_path>` fails; we need the old path for the
    // base content.
    let output = Command::new("git")
        .args(["diff", "-M", "--name-status", "--diff-filter=ACMR", diff_spec])
        .current_dir(repo_path)
        .output()
        .context("git diff --name-status")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }

    // Each line is: <status>\t<path> or <status>\t<old_path>\t<new_path> for renames.
    // Status R is followed by a similarity percentage (e.g. R100).
    struct FileEntry {
        /// Path to use for the new (HEAD) content and as the canonical name.
        new_path: String,
        /// Path to use when fetching base content. For renames this is the old path.
        base_path: String,
    }

    let changed_files: Vec<FileEntry> = std::str::from_utf8(&output.stdout)?
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let status = cols.next()?;
            if status.starts_with('R') {
                // Rename: old_path \t new_path
                let old = cols.next()?.to_string();
                let new = cols.next()?.to_string();
                Some(FileEntry { new_path: new, base_path: old })
            } else {
                // A/C/M: single path
                let path = cols.next()?.to_string();
                Some(FileEntry { new_path: path.clone(), base_path: path })
            }
        })
        .collect();

    // Get deleted files
    let del_output = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=D", diff_spec])
        .current_dir(repo_path)
        .output()
        .context("git diff --name-only --diff-filter=D")?;

    let deleted_files: Vec<&str> = std::str::from_utf8(&del_output.stdout)?
        .lines()
        .filter(|l| !l.is_empty())
        .collect();

    let mut all_changes = Vec::new();

    // Process modified/added/renamed files
    for entry in &changed_files {
        let path = Path::new(&entry.new_path);
        if nestweaver_parser::detect_language(path).is_none() {
            continue;
        }

        // Get old content from base ref using the base_path (handles renames).
        let old_output = Command::new("git")
            .args(["show", &format!("{}:{}", base_ref, entry.base_path)])
            .current_dir(repo_path)
            .output();

        let old_content = match old_output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => String::new(), // New file
        };

        // Get new content from HEAD (or working tree for the head side of the diff)
        let head_ref = diff_spec.split("..").nth(1).unwrap_or("HEAD");
        let new_output = Command::new("git")
            .args(["show", &format!("{}:{}", head_ref, entry.new_path)])
            .current_dir(repo_path)
            .output();

        let new_content = match new_output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => {
                // Fall back to working tree
                match std::fs::read_to_string(repo_path.join(&entry.new_path)) {
                    Ok(c) => c,
                    Err(_) => {
                        tracing::warn!(file = %entry.new_path, "diff impact: file not available via git show or working tree, skipping");
                        continue;
                    }
                }
            }
        };

        if old_content.is_empty() && new_content.is_empty() {
            continue;
        }

        match compute_file_changes(&old_content, &new_content, &entry.new_path, repo_url) {
            Ok(changes) => all_changes.extend(changes),
            Err(e) => {
                tracing::warn!(file = %entry.new_path, error = %e, "failed to diff file, skipping");
            }
        }
    }

    // Process deleted files (old content only)
    for file in &deleted_files {
        let path = Path::new(file);
        if nestweaver_parser::detect_language(path).is_none() {
            continue;
        }

        let old_output = Command::new("git")
            .args(["show", &format!("{}:{}", base_ref, file)])
            .current_dir(repo_path)
            .output();

        let old_content = match old_output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => continue,
        };

        match compute_file_changes(&old_content, "", file, repo_url) {
            Ok(changes) => all_changes.extend(changes),
            Err(e) => {
                tracing::warn!(file, error = %e, "failed to diff deleted file, skipping");
            }
        }
    }

    Ok(all_changes)
}

/// Run diff-based impact analysis using the local graph store.
///
/// This is the local-only path: parses the diff, computes atomic changes,
/// and queries the local store for impact. Used when no `--server` is provided.
pub fn run_diff_impact_local(
    config: &DiffImpactConfig,
    store: &nestweaver_store::GraphStore,
    max_depth: u32,
    include_tests: bool,
) -> Result<DiffImpactResult, anyhow::Error> {
    let changes = compute_diff_changes(config.repo_path, config.diff_spec, config.repo_url)?;

    if changes.is_empty() {
        return Ok(DiffImpactResult {
            changes: Vec::new(),
            impacts: Vec::new(),
            source: "local".to_string(),
            error: None,
        });
    }

    let impacts = crate::atomic_changes::analyze_impact(store, &changes, max_depth, include_tests)?;

    // Filter by minimum severity
    let impacts = filter_by_severity(impacts, config.min_severity);

    Ok(DiffImpactResult {
        changes,
        impacts,
        source: "local".to_string(),
        error: None,
    })
}

/// Filter impact results by minimum severity level.
pub fn filter_by_severity(impacts: Vec<ImpactResult>, min: ImpactSeverity) -> Vec<ImpactResult> {
    impacts
        .into_iter()
        .filter(|i| severity_ord(&i.severity) >= severity_ord(&min))
        .collect()
}

fn severity_ord(s: &ImpactSeverity) -> u8 {
    match s {
        ImpactSeverity::Breaking => 2,
        ImpactSeverity::Warning => 1,
        ImpactSeverity::Info => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_filter() {
        let impacts = vec![
            ImpactResult {
                change_canonical_id: "a".into(),
                change_kind: "SignatureChanged".into(),
                affected_canonical_id: "b".into(),
                affected_name: "foo".into(),
                affected_repo_url: "https://github.com/test/repo".into(),
                affected_file: "src/lib.rs".into(),
                affected_line: 10,
                affected_signature: "fn foo()".into(),
                severity: ImpactSeverity::Breaking,
                reason: "param added".into(),
            },
            ImpactResult {
                change_canonical_id: "c".into(),
                change_kind: "SymbolMoved".into(),
                affected_canonical_id: "d".into(),
                affected_name: "bar".into(),
                affected_repo_url: "https://github.com/test/repo".into(),
                affected_file: "src/bar.rs".into(),
                affected_line: 20,
                affected_signature: "fn bar()".into(),
                severity: ImpactSeverity::Warning,
                reason: "moved".into(),
            },
            ImpactResult {
                change_canonical_id: "e".into(),
                change_kind: "SymbolAdded".into(),
                affected_canonical_id: "f".into(),
                affected_name: "baz".into(),
                affected_repo_url: "https://github.com/test/repo".into(),
                affected_file: "src/baz.rs".into(),
                affected_line: 30,
                affected_signature: "fn baz()".into(),
                severity: ImpactSeverity::Info,
                reason: "added".into(),
            },
        ];

        let filtered = filter_by_severity(impacts.clone(), ImpactSeverity::Warning);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|i| i.severity != ImpactSeverity::Info));

        let filtered = filter_by_severity(impacts.clone(), ImpactSeverity::Breaking);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].severity, ImpactSeverity::Breaking);

        let filtered = filter_by_severity(impacts, ImpactSeverity::Info);
        assert_eq!(filtered.len(), 3);
    }
}
