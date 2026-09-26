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

use anyhow::Context;
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
/// Falls back to [`DEFAULT_PATTERNS`] only when no file exists.
///
/// Additional patterns from the `--ignore` CLI flag can be appended via
/// `extra_patterns`.
///
/// # Errors
///
/// Fails closed (nw-684): a `.brainignore` that exists but cannot be read, an
/// invalid glob (reported with its line number), or a pattern set that cannot
/// be built is an error naming the file. Callers must abort before writing
/// anything, because indexing without the user's exclusions would expose the
/// notes they excluded.
pub fn load_brain_ignore(vault_path: &Path, extra_patterns: &[String]) -> anyhow::Result<GlobSet> {
    let ignore_file = vault_path.join(".brainignore");
    // nw-684: an unreadable ignore file must never silently widen what is
    // indexed — the user wrote it to keep notes (credentials, private
    // folders) OUT of the graph. Only a truly absent file means defaults.
    let content = match std::fs::read_to_string(&ignore_file) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "cannot read {} — refusing to index this vault until it is readable, \
                     because indexing without it would expose notes it excludes",
                    ignore_file.display()
                )
            });
        }
    };
    build_ignore_set(&ignore_file, content.as_deref(), extra_patterns)
}

/// [`load_brain_ignore`] for content read through a [`ContentReader`] — the
/// server-mode vault path, whose reader is a bare clone with no working tree
/// (nw-684 review: `load_brain_ignore(reader.root())` stat'ed the bare path,
/// always found nothing, and so never honoured a committed `.brainignore`).
///
/// Only a positively absent file ([`ContentReader::read_optional_file`]
/// returning `None`) means defaults; any other failure is an error naming
/// the file.
///
/// [`ContentReader`]: crate::content_reader::ContentReader
/// [`ContentReader::read_optional_file`]: crate::content_reader::ContentReader::read_optional_file
pub fn load_brain_ignore_from_reader(
    reader: &dyn crate::content_reader::ContentReader,
    extra_patterns: &[String],
) -> anyhow::Result<GlobSet> {
    let rel = Path::new(".brainignore");
    let ignore_file = reader.root().join(rel);
    let content = reader.read_optional_file(rel).with_context(|| {
        format!(
            "cannot read {} — refusing to index this vault until it is readable, \
             because indexing without it would expose notes it excludes",
            ignore_file.display()
        )
    })?;
    build_ignore_set(&ignore_file, content.as_deref(), extra_patterns)
}

/// Build the ignore set from a `.brainignore`'s `content` (`None`: absent, so
/// the defaults) plus `extra_patterns`. Every invalid glob is an error naming
/// `ignore_file` and its line.
fn build_ignore_set(
    ignore_file: &Path,
    content: Option<&str>,
    extra_patterns: &[String],
) -> anyhow::Result<GlobSet> {
    let file_patterns: Vec<(Option<usize>, String)> = match content {
        Some(content) => parse_ignore_file(content)
            .into_iter()
            .map(|(n, p)| (Some(n), p))
            .collect(),
        None => default_ignore_patterns()
            .into_iter()
            .map(|p| (None, p))
            .collect(),
    };

    let mut builder = GlobSetBuilder::new();
    for (line, pattern) in file_patterns
        .into_iter()
        .chain(extra_patterns.iter().map(|p| (None, p.clone())))
    {
        let glob = Glob::new(&pattern).with_context(|| match line {
            Some(n) => format!(
                "invalid pattern on line {n} of {}: {pattern:?}",
                ignore_file.display()
            ),
            None => format!("invalid ignore pattern {pattern:?}"),
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .with_context(|| format!("build ignore pattern set for {}", ignore_file.display()))
}

/// Parse a `.brainignore` file's content into `(1-based line, pattern)` pairs.
/// Skips blank lines and lines starting with `#`.
fn parse_ignore_file(content: &str) -> Vec<(usize, String)> {
    content
        .lines()
        .enumerate()
        .map(|(i, line)| (i + 1, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(|(n, line)| (n, line.to_string()))
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

/// Test helpers shared by the indexer and watcher `.brainignore` tests.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Restores a file's mode on drop, so a failing assertion never leaves an
    /// unreadable file behind.
    pub(crate) struct RestoreMode(PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
        }
    }

    /// `chmod 000` the file. Returns `None` (after restoring it) when the
    /// file is still readable — running as root reads a 0o000 file anyway,
    /// so the caller must skip rather than pass hollow.
    pub(crate) fn make_unreadable(path: &Path) -> Option<RestoreMode> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let guard = RestoreMode(path.to_path_buf());
        if std::fs::read(path).is_ok() {
            eprintln!(
                "skipping: 0o000 {} is still readable (root?)",
                path.display()
            );
            return None;
        }
        Some(guard)
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ignore_file_skips_comments_and_blanks() {
        let content = "# comment\n\n*.backup.*\n  \n# another comment\nsnapshots/**\n";
        let patterns = parse_ignore_file(content);
        assert_eq!(
            patterns,
            vec![
                (3, "*.backup.*".to_string()),
                (6, "snapshots/**".to_string())
            ]
        );
    }

    #[test]
    fn default_patterns_match_expected_dirs() {
        let gs = load_brain_ignore(Path::new("/nonexistent"), &[]).unwrap();
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

        let gs = load_brain_ignore(vault, &[]).unwrap();
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
        let gs = load_brain_ignore(vault, &extra).unwrap();
        assert!(is_ignored("archive/old.md", &gs));
        assert!(is_ignored("drafts/wip.md", &gs));
    }

    #[test]
    fn extra_patterns_combined_with_defaults() {
        let gs = load_brain_ignore(Path::new("/nonexistent"), &["custom/**".to_string()]).unwrap();
        // Default still active.
        assert!(is_ignored(".obsidian/workspace.json", &gs));
        // Extra also active.
        assert!(is_ignored("custom/stuff.md", &gs));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_brainignore_is_an_error_not_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(".brainignore");
        std::fs::write(&f, "secret.md\n").unwrap();
        // Skips (returns None) when the 0o000 file is still readable, e.g.
        // running as root in a CI container: the precondition cannot exist.
        let Some(_restore) = test_support::make_unreadable(&f) else {
            return;
        };
        let err = load_brain_ignore(dir.path(), &[]).expect_err("must fail closed");
        let msg = format!("{err:#}");
        assert!(msg.contains(".brainignore"), "{msg}");
    }

    #[test]
    fn invalid_glob_is_an_error_naming_the_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".brainignore"), "ok/**\nfoo{a,b\n").unwrap();
        let msg = format!(
            "{:#}",
            load_brain_ignore(dir.path(), &[]).expect_err("invalid glob")
        );
        assert!(msg.contains("line 2") && msg.contains("foo{a,b"), "{msg}");
    }

    #[test]
    fn invalid_extra_pattern_is_an_error() {
        let msg = format!(
            "{:#}",
            load_brain_ignore(Path::new("/nonexistent"), &["bad{x".to_string()])
                .expect_err("invalid extra pattern")
        );
        assert!(msg.contains("bad{x"), "{msg}");
    }

    /// A reader over content with no working tree (nw-684 review). Only a
    /// typed `NotFound` means "absent"; any other read failure is an error.
    struct FailingReader(std::io::ErrorKind);
    impl crate::content_reader::ContentReader for FailingReader {
        fn read_file(&self, _rel_path: &Path) -> anyhow::Result<String> {
            Err(anyhow::Error::from(std::io::Error::from(self.0)).context("read blob"))
        }
        fn list_files(&self) -> anyhow::Result<Vec<std::path::PathBuf>> {
            Ok(Vec::new())
        }
        fn file_meta_nanos(&self, _rel_path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
            Ok(None)
        }
        fn root(&self) -> &Path {
            Path::new("/bare/vault.git")
        }
        fn version_id(&self) -> &str {
            "sha"
        }
    }

    #[test]
    fn reader_error_other_than_not_found_fails_closed() {
        let msg = format!(
            "{:#}",
            load_brain_ignore_from_reader(
                &FailingReader(std::io::ErrorKind::PermissionDenied),
                &[]
            )
            .expect_err("must fail closed")
        );
        assert!(msg.contains("/bare/vault.git/.brainignore"), "{msg}");
    }

    #[test]
    fn reader_not_found_means_defaults() {
        let gs = load_brain_ignore_from_reader(&FailingReader(std::io::ErrorKind::NotFound), &[])
            .unwrap();
        assert!(is_ignored(".obsidian/workspace.json", &gs));
    }

    #[test]
    fn missing_file_still_means_defaults() {
        let gs = load_brain_ignore(Path::new("/nonexistent"), &[]).unwrap();
        assert!(is_ignored(".obsidian/workspace.json", &gs));
    }
}
