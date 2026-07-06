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
    // base content. `-z` emits NUL-delimited records with paths verbatim
    // (no `core.quotePath` C-quoting), so accented/CJK filenames survive
    // instead of being emitted as `"caf\303\251.js"` and silently dropped
    // when handed to `git show`.
    let output = Command::new("git")
        .args([
            "diff",
            "-M",
            "--name-status",
            "-z",
            "--diff-filter=ACMR",
            diff_spec,
        ])
        .current_dir(repo_path)
        .output()
        .context("git diff --name-status")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }

    struct FileEntry {
        /// Path to use for the new (HEAD) content and as the canonical name.
        new_path: String,
        /// Path to use when fetching base content. For renames this is the old path.
        base_path: String,
    }

    // Under `-z` the record layout is NUL-separated tokens: `<status>\0<path>\0`
    // for A/C/M, and `R###\0<old>\0<new>\0` / `C###\0<old>\0<new>\0` for
    // renames/copies (two path tokens). Parse the token stream directly so
    // paths are never split on tab or newline embedded in a filename.
    let tokens: Vec<&[u8]> = output
        .stdout
        .split(|&b| b == 0)
        .filter(|t| !t.is_empty())
        .collect();
    let to_str = |b: &[u8]| String::from_utf8_lossy(b).into_owned();

    let mut changed_files: Vec<FileEntry> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let status = String::from_utf8_lossy(tokens[i]);
        i += 1;
        match status.chars().next().unwrap_or(' ') {
            // Rename/copy: two path tokens (old, new).
            'R' | 'C' => match (tokens.get(i), tokens.get(i + 1)) {
                (Some(old), Some(new)) => {
                    changed_files.push(FileEntry {
                        new_path: to_str(new),
                        base_path: to_str(old),
                    });
                    i += 2;
                }
                _ => break, // truncated stream; nothing more to consume
            },
            // Added/modified: single path token, same path for base and new.
            'A' | 'M' => {
                if let Some(path) = tokens.get(i) {
                    let p = to_str(path);
                    changed_files.push(FileEntry {
                        new_path: p.clone(),
                        base_path: p,
                    });
                    i += 1;
                } else {
                    break;
                }
            }
            // Unknown status: skip its (assumed single) path token to stay framed.
            _ => {
                if tokens.get(i).is_some() {
                    i += 1;
                }
            }
        }
    }

    // Get deleted files. `--name-only -z` emits just NUL-separated paths.
    let del_output = Command::new("git")
        .args(["diff", "--name-only", "-z", "--diff-filter=D", diff_spec])
        .current_dir(repo_path)
        .output()
        .context("git diff --name-only --diff-filter=D")?;

    let deleted_files: Vec<String> = del_output
        .stdout
        .split(|&b| b == 0)
        .filter(|t| !t.is_empty())
        .map(|b| String::from_utf8_lossy(b).into_owned())
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

        // For renames (base_path != new_path), emit SymbolMoved for symbols
        // that exist in both old and new content. Without this, cross-file moves
        // are invisible because compute_file_changes operates on a single path.
        if entry.base_path != entry.new_path && !old_content.is_empty() {
            match compute_rename_moves(
                &old_content,
                &new_content,
                &entry.base_path,
                &entry.new_path,
                repo_url,
            ) {
                Ok(moves) => all_changes.extend(moves),
                Err(e) => {
                    tracing::warn!(
                        old_file = %entry.base_path,
                        new_file = %entry.new_path,
                        error = %e,
                        "failed to compute rename moves, falling back to add/remove"
                    );
                }
            }
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

/// For a git-detected rename (old_path -> new_path), parse both sides and emit
/// `SymbolMoved` for every symbol that appears in both. This fills the gap
/// where `compute_file_changes` (single-file) cannot detect cross-file moves.
fn compute_rename_moves(
    old_content: &str,
    new_content: &str,
    old_path: &str,
    new_path: &str,
    repo_url: &str,
) -> Result<Vec<AtomicChange>, anyhow::Error> {
    let old_parsed = nestweaver_parser::parse_source(Path::new(old_path), old_content)?;
    let new_parsed = nestweaver_parser::parse_source(Path::new(new_path), new_content)?;

    // Build a set of (name, kind) for symbols in the new file.
    let new_symbols: std::collections::HashSet<(&str, nestweaver_schema::SymbolKind)> = new_parsed
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.kind))
        .collect();

    let mut moves = Vec::new();
    for old_sym in &old_parsed.symbols {
        if new_symbols.contains(&(old_sym.name.as_str(), old_sym.kind)) {
            let scope = old_sym.scope_chain.as_deref().unwrap_or("");
            let canonical_id = nestweaver_schema::uid::canonical_symbol_id(
                repo_url,
                old_path,
                &old_sym.name,
                scope,
            );
            moves.push(AtomicChange::SymbolMoved {
                canonical_id,
                name: old_sym.name.clone(),
                old_file: old_path.to_string(),
                new_file: new_path.to_string(),
            });
        }
    }
    Ok(moves)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_filter() {
        let impacts = vec![
            ImpactResult {
                change_canonical_id: "a".into(),
                change_kind: "SIGNATURE_CHANGED".into(),
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
                change_kind: "SYMBOL_MOVED".into(),
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
                change_kind: "SYMBOL_ADDED".into(),
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

    /// A source file with a non-ASCII (accented) name must survive diff parsing
    /// and produce atomic changes. Before the `-z` fix, `git diff --name-status`
    /// C-quoted the path (`"caf\303\251.rs"`), `git show` failed on that literal,
    /// and the file was silently dropped from impact analysis.
    #[test]
    fn compute_diff_changes_handles_non_ascii_paths() {
        use std::process::Command;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);

        // Base commit: an accented-name Rust file with one function.
        std::fs::write(repo.join("café.rs"), "fn greet() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);

        // Change the signature so a non-empty atomic change is produced.
        std::fs::write(repo.join("café.rs"), "fn greet(name: &str) {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "sig change"]);

        let changes =
            compute_diff_changes(repo, "HEAD~1..HEAD", "https://github.com/test/repo").unwrap();
        assert!(
            !changes.is_empty(),
            "accented-path file was dropped from diff impact: {changes:?}"
        );
    }

    /// A rename to a non-ASCII path must be parsed via the two-token `R` record
    /// under `-z` (old path + new path) rather than tab-split.
    #[test]
    fn compute_diff_changes_handles_non_ascii_rename() {
        use std::process::Command;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);

        std::fs::write(repo.join("old.rs"), "fn greet() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);

        git(&["mv", "old.rs", "café.rs"]);
        git(&["commit", "-m", "rename to accented"]);

        // Must not panic and must consume the two-token rename record cleanly.
        let changes =
            compute_diff_changes(repo, "HEAD~1..HEAD", "https://github.com/test/repo").unwrap();
        // A pure rename with no body change yields SymbolMoved for the function.
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, AtomicChange::SymbolMoved { new_file, .. } if new_file.contains("café"))),
            "expected a SymbolMoved into the accented path: {changes:?}"
        );
    }
}
