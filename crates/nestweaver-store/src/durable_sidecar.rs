//! Crash-safe persistence primitives for sidecars owned by the graph store.
//!
//! A successful replacement guarantees that the complete temp file reached
//! stable storage before it became canonical, and that the containing
//! directory was synced after the rename. A successful removal likewise syncs
//! the containing directory so the unlink cannot be lost across a crash.
//!
//! Existing targets retain the permissions representable by
//! [`std::fs::Permissions`]. On Unix, a newly-created sidecar requests mode
//! `0o666`, restricted by the process umask. Replacing a file cannot faithfully
//! copy every kind of metadata through portable `std` APIs: ownership, ACLs,
//! extended attributes, and platform-specific flags may be inherited from the
//! new file or its directory instead of from the old target.

use std::fs::{File, Permissions};
use std::io::{self, Write};
use std::path::Path;

fn sidecar_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn at_stage(stage: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{stage} {}: {error}", path.display()))
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowsDirectoryOpenSpec {
    access_mode: u32,
    share_mode: u32,
    custom_flags: u32,
}

#[cfg(any(windows, test))]
const fn windows_directory_open_spec() -> WindowsDirectoryOpenSpec {
    // Microsoft documents that FlushFileBuffers requires GENERIC_WRITE and
    // that CreateFileW needs FILE_FLAG_BACKUP_SEMANTICS to open a directory:
    // https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers
    // https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-createfilew
    // Sharing read, write, and delete avoids a directory-sync handle blocking
    // same-directory replacement.
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    WindowsDirectoryOpenSpec {
        access_mode: GENERIC_WRITE,
        share_mode: FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        custom_flags: FILE_FLAG_BACKUP_SEMANTICS,
    }
}

#[cfg(unix)]
fn sync_parent_directory_impl(path: &Path) -> io::Result<()> {
    File::open(sidecar_parent(path))?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory_impl(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let spec = windows_directory_open_spec();
    std::fs::OpenOptions::new()
        .access_mode(spec.access_mode)
        .share_mode(spec.share_mode)
        .custom_flags(spec.custom_flags)
        .open(sidecar_parent(path))?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_parent_directory_impl(path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "durable directory sync is unsupported for {}",
            sidecar_parent(path).display()
        ),
    ))
}

struct PersistFailure {
    error: io::Error,
    temp: tempfile::NamedTempFile,
}

trait DurableSidecarOps {
    fn sync_temp(&mut self, file: &File) -> io::Result<()>;
    fn persist(&mut self, temp: tempfile::NamedTempFile, path: &Path)
    -> Result<(), PersistFailure>;
    fn sync_parent(&mut self, path: &Path) -> io::Result<()>;
    fn remove(&mut self, path: &Path) -> io::Result<()>;
}

struct FileSystemOps;

impl DurableSidecarOps for FileSystemOps {
    fn sync_temp(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }

    fn persist(
        &mut self,
        temp: tempfile::NamedTempFile,
        path: &Path,
    ) -> Result<(), PersistFailure> {
        temp.persist(path)
            .map(|_| ())
            .map_err(|failure| PersistFailure {
                error: failure.error,
                temp: failure.file,
            })
    }

    fn sync_parent(&mut self, path: &Path) -> io::Result<()> {
        sync_parent_directory_durable(path)
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// Durably publish a namespace change to `path` by syncing its containing
/// directory. `path` itself need not exist.
pub fn sync_parent_directory_durable(path: &Path) -> io::Result<()> {
    sync_parent_directory_impl(path)
        .map_err(|error| at_stage("sync sidecar parent for", path, error))
}

fn existing_permissions(path: &Path) -> io::Result<Option<Permissions>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn create_temp(path: &Path, existing: Option<&Permissions>) -> io::Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    if let Some(permissions) = existing {
        builder.permissions(permissions.clone());
    }
    #[cfg(unix)]
    if existing.is_none() {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(Permissions::from_mode(0o666));
    }
    builder.tempfile_in(sidecar_parent(path))
}

fn atomic_replace_file_with_ops(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
    ops: &mut dyn DurableSidecarOps,
) -> io::Result<()> {
    let permissions = existing_permissions(path)
        .map_err(|error| at_stage("read target permissions for", path, error))?;
    let mut temp = create_temp(path, permissions.as_ref())
        .map_err(|error| at_stage("create sidecar temp for", path, error))?;
    write(temp.as_file_mut()).map_err(|error| at_stage("write sidecar temp for", path, error))?;
    temp.as_file_mut()
        .flush()
        .map_err(|error| at_stage("flush sidecar temp for", path, error))?;
    if let Some(permissions) = permissions {
        temp.as_file()
            .set_permissions(permissions)
            .map_err(|error| at_stage("apply target permissions to", path, error))?;
    }
    ops.sync_temp(temp.as_file())
        .map_err(|error| at_stage("sync sidecar temp for", path, error))?;
    if let Err(failure) = ops.persist(temp, path) {
        let error = at_stage("replace sidecar", path, failure.error);
        drop(failure.temp);
        return Err(error);
    }
    ops.sync_parent(path)
        .map_err(|error| at_stage("sync parent after replacing", path, error))
}

fn remove_file_durable_with_ops(
    path: &Path,
    ignore_not_found: bool,
    ops: &mut dyn DurableSidecarOps,
) -> io::Result<bool> {
    match ops.remove(path) {
        Ok(()) => {
            ops.sync_parent(path)
                .map_err(|error| at_stage("sync parent after unlinking", path, error))?;
            Ok(true)
        }
        Err(error) if ignore_not_found && error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(at_stage("unlink sidecar", path, error)),
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestFault {
    TempSync,
    Persist,
    ParentSync,
    Remove,
}

#[cfg(test)]
thread_local! {
    static TEST_FAULT: std::cell::Cell<Option<TestFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_fault<T>(fault: TestFault, action: impl FnOnce() -> T) -> T {
    TEST_FAULT.with(|active| {
        let previous = active.replace(Some(fault));
        let result = action();
        active.set(previous);
        result
    })
}

#[cfg(test)]
struct FaultOps {
    inner: FileSystemOps,
    fault: TestFault,
}

#[cfg(test)]
impl DurableSidecarOps for FaultOps {
    fn sync_temp(&mut self, file: &File) -> io::Result<()> {
        if self.fault == TestFault::TempSync {
            Err(io::Error::other("injected temp sync failure"))
        } else {
            self.inner.sync_temp(file)
        }
    }

    fn persist(
        &mut self,
        temp: tempfile::NamedTempFile,
        path: &Path,
    ) -> Result<(), PersistFailure> {
        if self.fault == TestFault::Persist {
            Err(PersistFailure {
                error: io::Error::other("injected persist failure"),
                temp,
            })
        } else {
            self.inner.persist(temp, path)
        }
    }

    fn sync_parent(&mut self, path: &Path) -> io::Result<()> {
        if self.fault == TestFault::ParentSync {
            Err(io::Error::other("injected parent sync failure"))
        } else {
            self.inner.sync_parent(path)
        }
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        if self.fault == TestFault::Remove {
            Err(io::Error::other("injected remove failure"))
        } else {
            self.inner.remove(path)
        }
    }
}

fn atomic_replace_file_impl(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    #[cfg(test)]
    if let Some(fault) = TEST_FAULT.with(std::cell::Cell::get) {
        return atomic_replace_file_with_ops(
            path,
            write,
            &mut FaultOps {
                inner: FileSystemOps,
                fault,
            },
        );
    }
    atomic_replace_file_with_ops(path, write, &mut FileSystemOps)
}

/// Atomically replace `path` with the bytes produced by `write`, then make the
/// replacement durable by syncing both the temp file and its parent directory.
///
/// An error from the final parent-directory sync means the complete canonical
/// file may already have replaced the old path, but crash durability of that
/// namespace change was not confirmed.
pub fn atomic_replace_file(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    atomic_replace_file_impl(path, write)
}

fn remove_file_durable_impl(path: &Path, ignore_not_found: bool) -> io::Result<bool> {
    #[cfg(test)]
    if let Some(fault) = TEST_FAULT.with(std::cell::Cell::get) {
        return remove_file_durable_with_ops(
            path,
            ignore_not_found,
            &mut FaultOps {
                inner: FileSystemOps,
                fault,
            },
        );
    }
    remove_file_durable_with_ops(path, ignore_not_found, &mut FileSystemOps)
}

/// Remove `path` and durably publish the unlink by syncing its parent.
///
/// Unlike [`remove_file_durable_if_exists`], a missing file is returned as a
/// `NotFound` error. An error from the final parent-directory sync means the
/// path may already be unlinked, but crash durability was not confirmed.
pub fn remove_file_durable(path: &Path) -> io::Result<()> {
    remove_file_durable_impl(path, false).map(|_| ())
}

/// Remove `path` when present and durably publish the unlink. Returns `true`
/// when a file was removed and `false` when it was already absent.
///
/// An error from the final parent-directory sync means the path may already be
/// unlinked, but crash durability of that namespace change was not confirmed.
pub fn remove_file_durable_if_exists(path: &Path) -> io::Result<bool> {
    remove_file_durable_impl(path, true)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{
        FileSystemOps, TestFault, WindowsDirectoryOpenSpec, atomic_replace_file,
        atomic_replace_file_with_ops, remove_file_durable, remove_file_durable_if_exists,
        windows_directory_open_spec, with_test_fault,
    };
    use crate::GraphStore;

    fn make_symbol(uid: &str) -> nestweaver_schema::Symbol {
        nestweaver_schema::Symbol {
            uid: uid.to_string(),
            name: uid.to_string(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo-1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {uid}()"),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn only_entry_count(path: &Path) -> usize {
        std::fs::read_dir(path).unwrap().count()
    }

    use std::path::Path;

    #[test]
    fn windows_directory_sync_requests_flushable_shared_directory_handle() {
        assert_eq!(
            windows_directory_open_spec(),
            WindowsDirectoryOpenSpec {
                access_mode: 0x4000_0000,
                share_mode: 0x0000_0001 | 0x0000_0002 | 0x0000_0004,
                custom_flags: 0x0200_0000,
            }
        );
    }

    #[test]
    fn atomic_replace_preserves_canonical_and_cleans_temp_on_partial_write_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generation");
        std::fs::write(&path, b"41").unwrap();

        let error = atomic_replace_file(&path, |file| {
            file.write_all(b"4")?;
            Err(std::io::Error::other("injected partial write failure"))
        })
        .unwrap_err();

        assert!(error.to_string().contains("write sidecar temp for"));
        assert!(error.to_string().contains("injected partial write failure"));
        assert_eq!(std::fs::read(&path).unwrap(), b"41");
        assert_eq!(only_entry_count(dir.path()), 1);
    }

    #[test]
    fn operation_faults_wrap_actual_filesystem_operations_and_clean_temps() {
        for fault in [TestFault::TempSync, TestFault::Persist] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("generation");
            std::fs::write(&path, b"41").unwrap();

            let error = with_test_fault(fault, || {
                atomic_replace_file(&path, |file| file.write_all(b"42"))
            })
            .unwrap_err();

            assert!(error.to_string().contains("injected"));
            assert_eq!(std::fs::read(&path).unwrap(), b"41", "{fault:?}");
            assert_eq!(only_entry_count(dir.path()), 1, "{fault:?}");
        }
    }

    #[test]
    fn genuine_persist_failure_cleans_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target-directory");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"keep").unwrap();

        let error = atomic_replace_file_with_ops(
            &path,
            |file| file.write_all(b"replacement"),
            &mut FileSystemOps,
        )
        .unwrap_err();

        assert!(error.to_string().contains("replace sidecar"));
        assert_eq!(only_entry_count(dir.path()), 1);
        assert_eq!(std::fs::read(path.join("occupied")).unwrap(), b"keep");
    }

    #[test]
    fn atomic_replace_parent_sync_failure_leaves_complete_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generation");
        std::fs::write(&path, b"41").unwrap();

        let error = with_test_fault(TestFault::ParentSync, || {
            atomic_replace_file(&path, |file| file.write_all(b"42"))
        })
        .unwrap_err();

        assert!(error.to_string().contains("sync parent after replacing"));
        assert_eq!(std::fs::read(&path).unwrap(), b"42");
        assert_eq!(only_entry_count(dir.path()), 1);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replace_preserves_existing_unix_modes_and_read_only_state() {
        use std::os::unix::fs::PermissionsExt;

        for mode in [0o640, 0o644, 0o444] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("generation");
            std::fs::write(&path, b"41").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();

            atomic_replace_file(&path, |file| file.write_all(b"42")).unwrap();

            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                mode
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"42");
        }
    }

    #[cfg(unix)]
    #[test]
    fn initial_create_matches_normal_file_umask_semantics() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let reference = dir.path().join("reference");
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o666)
            .open(&reference)
            .unwrap();
        let path = dir.path().join("generation");

        atomic_replace_file(&path, |file| file.write_all(b"1")).unwrap();

        let expected = std::fs::metadata(reference).unwrap().permissions().mode() & 0o777;
        let actual = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(actual, expected);
    }

    #[test]
    fn public_generation_save_failure_reopens_old_complete_value() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = dir.path().join("test.lbug.generation");
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store.bump_graph_generation();
            store.save_graph_generation(&generation_path).unwrap();
            store.bump_graph_generation();
            let error = with_test_fault(TestFault::Persist, || {
                store.save_graph_generation(&generation_path)
            })
            .unwrap_err();
            assert!(error.to_string().contains("replace sidecar"));
        }

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(reopened.graph_generation(), 1);
        assert_eq!(std::fs::read_to_string(generation_path).unwrap(), "1");
    }

    #[test]
    fn public_pagerank_save_failure_reopens_old_complete_value() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let pagerank_path = dir.path().join("test.lbug.pagerank.json");
        let old_bytes;
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store.insert_symbol(&make_symbol("old")).unwrap();
            store
                .compute_pagerank(0.85, 20, &crate::ranking::GraphScope::code_only())
                .unwrap();
            store.save_pagerank_cache(&pagerank_path).unwrap();
            old_bytes = std::fs::read(&pagerank_path).unwrap();
            store.insert_symbol(&make_symbol("new")).unwrap();
            store
                .compute_pagerank(0.85, 20, &crate::ranking::GraphScope::code_only())
                .unwrap();
            let error = with_test_fault(TestFault::Persist, || {
                store.save_pagerank_cache(&pagerank_path)
            })
            .unwrap_err();
            assert!(error.to_string().contains("replace sidecar"));
            assert_eq!(std::fs::read(&pagerank_path).unwrap(), old_bytes);
        }

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert_eq!(reopened.pagerank_scores().unwrap().get("old"), Some(&1.0));
        assert!(!reopened.pagerank_scores().unwrap().contains_key("new"));
    }

    #[test]
    fn durable_remove_faults_replace_actual_remove_and_parent_sync_operations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pagerank.json");
        std::fs::write(&path, b"stale").unwrap();

        let error = with_test_fault(TestFault::Remove, || remove_file_durable(&path)).unwrap_err();
        assert!(error.to_string().contains("unlink sidecar"));
        assert_eq!(std::fs::read(&path).unwrap(), b"stale");

        let error =
            with_test_fault(TestFault::ParentSync, || remove_file_durable(&path)).unwrap_err();
        assert!(error.to_string().contains("sync parent after unlinking"));
        assert!(!path.exists());
    }

    #[test]
    fn durable_remove_not_found_policy_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");

        assert!(!remove_file_durable_if_exists(&path).unwrap());
        assert_eq!(
            remove_file_durable(&path).unwrap_err().kind(),
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
            store.insert_symbol(&make_symbol("stale")).unwrap();
            store
                .compute_pagerank(0.85, 20, &crate::ranking::GraphScope::code_only())
                .unwrap();
            store.save_pagerank_cache(&pagerank_path).unwrap();
            assert!(store.pagerank_scores().unwrap().contains_key("stale"));
        }

        assert!(remove_file_durable_if_exists(&pagerank_path).unwrap());

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(
            reopened.pagerank_cache.lock().unwrap().is_none(),
            "a missing sidecar must not populate ranks; a later query may legitimately recompute from the graph"
        );
        assert!(!pagerank_path.exists());
    }
}
