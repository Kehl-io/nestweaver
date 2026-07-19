//! Crash-safe persistence primitives for sidecars owned by the graph store.
//!
//! A successful replacement guarantees that the complete temp file reached
//! stable storage before it became canonical, and that the containing
//! directory was synced after the rename. A successful removal likewise syncs
//! the containing directory so the unlink cannot be lost across a crash.

use std::io::Write;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtomicReplaceStep {
    TempSync,
    Rename,
    ParentSync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoveStep {
    Unlink,
    ParentSync,
}

fn sidecar_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(sidecar_parent(path))?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(sidecar_parent(path))?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "durable directory sync is unsupported for {}",
            sidecar_parent(path).display()
        ),
    ))
}

fn atomic_replace_file_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    mut before: impl FnMut(AtomicReplaceStep) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(sidecar_parent(path))?;
    write(temp.as_file_mut())?;
    temp.as_file_mut().flush()?;
    before(AtomicReplaceStep::TempSync)?;
    temp.as_file().sync_all()?;
    before(AtomicReplaceStep::Rename)?;
    temp.persist(path).map_err(|error| error.error)?;
    before(AtomicReplaceStep::ParentSync)?;
    sync_parent_directory(path)
}

/// Atomically replace `path` with the bytes produced by `write`, then make the
/// replacement durable by syncing both the temp file and its parent directory.
pub fn atomic_replace_file(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    atomic_replace_file_with(path, write, |_| Ok(()))
}

fn remove_file_durable_with(
    path: &Path,
    ignore_not_found: bool,
    mut before: impl FnMut(RemoveStep) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    before(RemoveStep::Unlink)?;
    match std::fs::remove_file(path) {
        Ok(()) => {
            before(RemoveStep::ParentSync)?;
            sync_parent_directory(path)?;
            Ok(true)
        }
        Err(error) if ignore_not_found && error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Remove `path` and durably publish the unlink by syncing its parent.
///
/// Unlike [`remove_file_durable_if_exists`], a missing file is returned as a
/// `NotFound` error so callers retain control of their existing missing-file
/// policy.
pub fn remove_file_durable(path: &Path) -> std::io::Result<()> {
    remove_file_durable_with(path, false, |_| Ok(())).map(|_| ())
}

/// Remove `path` when present and durably publish the unlink. Returns `true`
/// when a file was removed and `false` when it was already absent.
pub fn remove_file_durable_if_exists(path: &Path) -> std::io::Result<bool> {
    remove_file_durable_with(path, true, |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{AtomicReplaceStep, atomic_replace_file_with, remove_file_durable_with};
    use crate::GraphStore;

    fn fail_at(target: AtomicReplaceStep) -> impl FnMut(AtomicReplaceStep) -> std::io::Result<()> {
        move |step| {
            if step == target {
                Err(std::io::Error::other(format!("injected {step:?} failure")))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn atomic_replace_preserves_canonical_and_cleans_temp_on_partial_write_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generation");
        std::fs::write(&path, b"41").unwrap();

        let error = atomic_replace_file_with(
            &path,
            |file| {
                file.write_all(b"4")?;
                Err(std::io::Error::other("injected partial write failure"))
            },
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("injected partial write failure"));
        assert_eq!(std::fs::read(&path).unwrap(), b"41");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_replace_preserves_canonical_before_rename_and_cleans_temp() {
        for step in [AtomicReplaceStep::TempSync, AtomicReplaceStep::Rename] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("generation");
            std::fs::write(&path, b"41").unwrap();

            let error =
                atomic_replace_file_with(&path, |file| file.write_all(b"42"), fail_at(step))
                    .unwrap_err();

            assert!(error.to_string().contains("injected"));
            assert_eq!(std::fs::read(&path).unwrap(), b"41", "failed at {step:?}");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn atomic_replace_parent_sync_failure_leaves_complete_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generation");
        std::fs::write(&path, b"41").unwrap();

        let error = atomic_replace_file_with(
            &path,
            |file| file.write_all(b"42"),
            fail_at(AtomicReplaceStep::ParentSync),
        )
        .unwrap_err();

        assert!(error.to_string().contains("injected"));
        assert_eq!(std::fs::read(&path).unwrap(), b"42");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_replace_success_publishes_only_complete_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generation");
        std::fs::write(&path, b"41").unwrap();

        atomic_replace_file_with(&path, |file| file.write_all(b"42"), |_| Ok(())).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"42");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn generation_reopen_after_pre_rename_failure_reads_old_complete_value() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = dir.path().join("test.lbug.generation");
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store.bump_graph_generation();
            store.save_graph_generation(&generation_path).unwrap();
        }

        atomic_replace_file_with(
            &generation_path,
            |file| file.write_all(b"999"),
            fail_at(AtomicReplaceStep::Rename),
        )
        .unwrap_err();

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(reopened.graph_generation(), 1);
        assert_eq!(std::fs::read_to_string(&generation_path).unwrap(), "1");
    }

    #[test]
    fn durable_remove_preserves_file_when_unlink_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pagerank.json");
        std::fs::write(&path, b"stale").unwrap();

        let error = remove_file_durable_with(&path, false, |step| {
            if step == super::RemoveStep::Unlink {
                Err(std::io::Error::other("injected unlink failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected unlink failure"));
        assert_eq!(std::fs::read(&path).unwrap(), b"stale");
    }

    #[test]
    fn durable_remove_parent_sync_failure_reports_error_after_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pagerank.json");
        std::fs::write(&path, b"stale").unwrap();

        let error = remove_file_durable_with(&path, false, |step| {
            if step == super::RemoveStep::ParentSync {
                Err(std::io::Error::other("injected unlink parent sync failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected unlink parent sync failure")
        );
        assert!(!path.exists());
    }

    #[test]
    fn durable_remove_not_found_policy_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");

        assert!(!remove_file_durable_with(&path, true, |_| Ok(())).unwrap());
        assert_eq!(
            remove_file_durable_with(&path, false, |_| Ok(()))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn stale_pagerank_cannot_reload_after_durable_remove_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let pagerank_path = dir.path().join("test.lbug.pagerank.json");
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            std::fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
            store.load_pagerank_cache(&pagerank_path).unwrap();
            assert!(store.pagerank_scores().contains_key("stale"));
        }

        assert!(remove_file_durable_with(&pagerank_path, true, |_| Ok(())).unwrap());

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!reopened.pagerank_scores().contains_key("stale"));
        assert!(!pagerank_path.exists());
    }
}
