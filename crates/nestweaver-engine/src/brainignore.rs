//! `.brainignore` support for vault indexing exclusion patterns.
//!
//! When indexing a markdown vault (`brain add`, `brain refresh`, `brain watch`),
//! the indexer checks for a `.brainignore` file at the vault root. Each
//! non-blank, non-comment line is a glob pattern matched against the file's
//! path relative to the vault root. Matched files are skipped before parsing.
//!
//! If no `.brainignore` file exists, a set of sensible defaults is applied
//! (runtime backups, snapshots, `.obsidian`, etc.). The file follows the same
//! line format as `.gitignore` (one pattern per line, `#` comments, blank lines
//! ignored) but uses `globset` semantics rather than full gitignore semantics.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Default patterns applied when no `.brainignore` file is present.
const DEFAULT_PATTERNS: &[&str] = &[
    "**/.runtime-backups*/**",
    "**/*.backup.*/**",
    "**/snapshots/**",
    "**/.obsidian/**",
    "**/node_modules/**",
    "**/.git/**",
    "**/.trash/**",
    "**/target/**",
    "**/.next/**",
    "**/.nuxt/**",
    "**/dist/**",
    "**/build/**",
];

/// Load ignore patterns from a `.brainignore` file in the vault root.
/// Falls back to [`DEFAULT_PATTERNS`] when no file exists.
///
/// Additional patterns from the `--ignore` CLI flag can be appended via
/// `extra_patterns`.
pub fn load_brain_ignore(vault_path: &Path, extra_patterns: &[String]) -> GlobSet {
    let ignore_file = vault_path.join(".brainignore");
    let file_patterns: Vec<String> = if ignore_file.exists() {
        match std::fs::read_to_string(&ignore_file) {
            Ok(content) => parse_ignore_file(&content),
            Err(e) => {
                tracing::warn!(
                    path = %ignore_file.display(),
                    error = %e,
                    "failed to read .brainignore; using defaults"
                );
                default_ignore_patterns()
            }
        }
    } else {
        default_ignore_patterns()
    };

    let mut builder = GlobSetBuilder::new();
    for pattern in file_patterns.iter().chain(extra_patterns.iter()) {
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(e) => {
                tracing::warn!(pattern = %pattern, error = %e, "invalid brainignore glob pattern");
            }
        }
    }
    builder.build().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to build brainignore GlobSet; no patterns active");
        GlobSet::empty()
    })
}

/// Parse a `.brainignore` file's content into a list of glob patterns.
/// Skips blank lines and lines starting with `#`.
fn parse_ignore_file(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.to_string())
        .collect()
}

/// Return the default ignore patterns as owned strings.
fn default_ignore_patterns() -> Vec<String> {
    DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect()
}

/// Check whether a relative path should be ignored according to the given
/// `GlobSet`. The path should be relative to the vault root, using
/// forward slashes.
pub fn is_ignored(rel_path: &str, ignore_set: &GlobSet) -> bool {
    ignore_set.is_match(rel_path)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ignore_file_skips_comments_and_blanks() {
        let content = "# comment\n\n*.backup.*\n  \n# another comment\nsnapshots/**\n";
        let patterns = parse_ignore_file(content);
        assert_eq!(patterns, vec!["*.backup.*", "snapshots/**"]);
    }

    #[test]
    fn default_patterns_match_expected_dirs() {
        let gs = load_brain_ignore(Path::new("/nonexistent"), &[]);
        assert!(is_ignored(".obsidian/workspace.json", &gs));
        assert!(is_ignored("node_modules/foo/bar.md", &gs));
        assert!(is_ignored(".git/HEAD", &gs));
        assert!(is_ignored(".trash/deleted.md", &gs));
        assert!(is_ignored("target/debug/build.md", &gs));
        assert!(is_ignored("sub/.runtime-backups-2026/file.md", &gs));
        assert!(is_ignored("foo/snapshots/snap.md", &gs));
        // Normal notes should NOT match.
        assert!(!is_ignored("notes/real.md", &gs));
        assert!(!is_ignored("projects/todo.md", &gs));
    }

    #[test]
    fn custom_brainignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path();
        std::fs::write(
            vault.join(".brainignore"),
            "# My custom ignore\n*.backup.*\nmirror/**\n",
        )
        .unwrap();

        let gs = load_brain_ignore(vault, &[]);
        assert!(is_ignored("notes.backup.20260527/real.md", &gs));
        assert!(is_ignored("mirror/sub/file.md", &gs));
        // Default patterns should NOT be active when a custom file exists.
        assert!(!is_ignored(".obsidian/workspace.json", &gs));
    }

    #[test]
    fn extra_patterns_combined_with_file() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path();
        std::fs::write(vault.join(".brainignore"), "archive/**\n").unwrap();

        let extra = vec!["drafts/**".to_string()];
        let gs = load_brain_ignore(vault, &extra);
        assert!(is_ignored("archive/old.md", &gs));
        assert!(is_ignored("drafts/wip.md", &gs));
    }

    #[test]
    fn extra_patterns_combined_with_defaults() {
        let gs = load_brain_ignore(Path::new("/nonexistent"), &["custom/**".to_string()]);
        // Default still active.
        assert!(is_ignored(".obsidian/workspace.json", &gs));
        // Extra also active.
        assert!(is_ignored("custom/stuff.md", &gs));
    }
}
