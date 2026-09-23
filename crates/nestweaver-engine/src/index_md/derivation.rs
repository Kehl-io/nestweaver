//! Rebuild legacy link derivation without expanding the indexed source scope.

use super::*;
use std::collections::BTreeMap;
use std::path::Component;
use std::time::Duration;

pub const MARKDOWN_LINK_DERIVATION_VERSION: u32 = 1;
const MAX_CAPTURE_BYTES: usize = 256 * 1024 * 1024;
const MAX_CAPTURE_NOTES: usize = 100_000;
const PREPARATION_BUDGET: Duration = Duration::from_secs(60);

fn migration_ignore(root: &Path) -> anyhow::Result<GlobSet> {
    use std::io::Read;
    let path = root.join(".brainignore");
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(crate::brainignore::load_brain_ignore(root, &[]));
        }
        Err(error) => return Err(error).context("read vault derivation ignore policy"),
    };
    let mut content = String::new();
    (&mut file)
        .take(64 * 1024 + 1)
        .read_to_string(&mut content)?;
    anyhow::ensure!(
        content.len() <= 64 * 1024,
        "vault ignore policy exceeds migration budget"
    );
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        builder.add(globset::Glob::new(pattern).context("invalid vault ignore policy")?);
    }
    Ok(builder.build()?)
}

struct CapturedNotes {
    root: PathBuf,
    sources: BTreeMap<PathBuf, String>,
    limit: u64,
}

impl ContentReader for CapturedNotes {
    fn read_file(&self, path: &Path) -> anyhow::Result<String> {
        self.sources
            .get(path)
            .cloned()
            .context("note outside captured indexed inventory")
    }
    fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        Ok(self.sources.keys().cloned().collect())
    }
    fn file_meta_nanos(&self, path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
        Ok(Some((
            0,
            self.sources
                .get(path)
                .context("unknown captured note")?
                .len() as u64,
        )))
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn version_id(&self) -> &str {
        "captured-markdown-derivation-v1"
    }
    fn max_source_file_bytes(&self) -> u64 {
        self.limit
    }
}

fn checked_relative(path: &str, has_file: &dyn Fn(&Path) -> bool) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(path);
    anyhow::ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "legacy note path is not a canonical relative path"
    );
    anyhow::ensure!(
        is_markdown(&path) && !path_has_vault_skip_dir(&path, has_file),
        "indexed note is excluded by the current vault policy"
    );
    Ok(path)
}

fn capture_notes(
    reader: &dyn ContentReader,
    paths: &[String],
    ignore_set: &GlobSet,
    check: &impl Fn() -> anyhow::Result<()>,
) -> anyhow::Result<CapturedNotes> {
    anyhow::ensure!(
        paths.len() <= MAX_CAPTURE_NOTES,
        "vault derivation note budget exceeded"
    );
    // Enumeration applies the provider's current eligibility without reading
    // unindexed source bodies. Any previously indexed exclusion blocks this
    // migration instead of becoming a deletion or a scope expansion.
    let eligible: HashSet<_> = reader.list_files()?.into_iter().collect();
    let mut sources = BTreeMap::new();
    let mut total = 0usize;
    for path in paths {
        check()?;
        let relative = checked_relative(path, &|probe| reader.has_file(probe))?;
        anyhow::ensure!(
            eligible.contains(&relative)
                && reader.accepts_path(&relative)
                && !crate::brainignore::is_ignored(path, ignore_set),
            "indexed note is unavailable or excluded: {path}"
        );
        let source = reader
            .read_file(&relative)
            .with_context(|| format!("read indexed note {path}"))?;
        anyhow::ensure!(
            source.len() as u64 <= reader.max_source_file_bytes(),
            "indexed note exceeds current size limit: {path}"
        );
        total = total
            .checked_add(source.len())
            .context("vault derivation size overflow")?;
        anyhow::ensure!(
            total <= MAX_CAPTURE_BYTES,
            "vault derivation byte budget exceeded"
        );
        // The ordinary full indexer tolerates parse failures. A migration may
        // not convert one into missing rows, so reject before entering it.
        parse_markdown(path, &source).with_context(|| format!("parse indexed note {path}"))?;
        anyhow::ensure!(
            sources.insert(relative, source).is_none(),
            "duplicate indexed note path: {path}"
        );
    }
    check()?;
    Ok(CapturedNotes {
        root: reader.root().to_path_buf(),
        sources,
        limit: reader.max_source_file_bytes(),
    })
}

/// Refresh every already-indexed note using the current link algorithm.
///
/// The caller must hold the daemon's writer and shutdown ownership throughout
/// this call and establish search debt first. This function returns the full
/// publication outcome; it never certifies a durable migration record. The
/// caller must finish search/publication reconciliation before doing so.
pub fn refresh_indexed_markdown_derivation(
    reader: &dyn ContentReader,
    store: &GraphStore,
    vault: &Vault,
    cancelled: impl Fn() -> bool,
) -> anyhow::Result<MarkdownRefreshResult> {
    let started = Instant::now();
    let check = || {
        anyhow::ensure!(!cancelled(), "vault derivation preparation cancelled");
        anyhow::ensure!(
            started.elapsed() < PREPARATION_BUDGET,
            "vault derivation preparation budget exceeded"
        );
        Ok(())
    };
    check()?;
    anyhow::ensure!(
        vault_uid(&vault.instance_id, &reader.root().to_string_lossy()) == vault.uid,
        "vault derivation source identity mismatch"
    );
    let (notes, integrity) = store.with_read_deadline(started + PREPARATION_BUDGET, || {
        store.list_notes_with_integrity(Some(&vault.uid))
    })?;
    anyhow::ensure!(
        integrity.is_complete(),
        "vault derivation inventory is incomplete"
    );
    let mut paths = Vec::with_capacity(notes.len());
    for note in notes {
        anyhow::ensure!(
            note.vault_uid == vault.uid && note.uid == note_uid(&vault.uid, &note.file_path),
            "vault derivation note identity mismatch"
        );
        paths.push(note.file_path);
    }
    let ignore_set = migration_ignore(reader.root())?;
    let captured = capture_notes(reader, &paths, &ignore_set, &check)?;
    index_into_store_with_write_gate(
        &captured,
        store,
        &vault.instance_id,
        &vault.name,
        &ignore_set,
        None,
        || {
            // Capture again before the transaction: source changes must not
            // let an older snapshot certify the current indexed inventory.
            let current_ignore = migration_ignore(reader.root())?;
            let current = capture_notes(reader, &paths, &current_ignore, &check)?;
            anyhow::ensure!(
                current.sources == captured.sources,
                "vault source changed during derivation preparation"
            );
            check()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_capture_rejects_noncanonical_and_excluded_paths() {
        for path in [
            "",
            "/a.md",
            "../a.md",
            "a/../b.md",
            "./a.md",
            ".obsidian/a.md",
            "code.rs",
        ] {
            assert!(checked_relative(path, &|_| false).is_err(), "{path}");
        }
        assert_eq!(
            checked_relative("notes/a.md", &|_| false).unwrap(),
            Path::new("notes/a.md")
        );
    }

    #[test]
    fn legacy_capture_never_discovers_unindexed_notes_and_refuses_missing_source() {
        let reader = CapturedNotes {
            root: PathBuf::from("/fixture"),
            limit: 1024,
            sources: [
                (PathBuf::from("a.md"), "[[B#Missing]]".into()),
                (PathBuf::from("excluded.md"), "canary".into()),
            ]
            .into(),
        };
        let captured =
            capture_notes(&reader, &["a.md".into()], &GlobSet::empty(), &|| Ok(())).unwrap();
        assert_eq!(captured.sources.len(), 1);
        assert!(!captured.sources.contains_key(Path::new("excluded.md")));
        assert!(
            capture_notes(&reader, &["missing.md".into()], &GlobSet::empty(), &|| Ok(
                ()
            ))
            .is_err()
        );
        assert!(
            capture_notes(
                &reader,
                &["a.md".into(), "a.md".into()],
                &GlobSet::empty(),
                &|| Ok(())
            )
            .is_err()
        );
        assert!(
            capture_notes(
                &reader,
                &["a.md".into()],
                &GlobSet::empty(),
                &|| anyhow::bail!("cancelled")
            )
            .is_err()
        );
    }
}
