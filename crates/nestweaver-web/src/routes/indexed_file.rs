use std::fs::File;
use std::io;
use std::path::{Component, Path};

/// A stored file name beneath a separately trusted, registered root.
pub(super) struct IndexedFilePath<'a>(&'a Path);

impl<'a> IndexedFilePath<'a> {
    pub(super) fn new(path: &'a str) -> Option<Self> {
        let relative = Path::new(path);
        if path.is_empty()
            || path.starts_with('\\')
            || path.contains('\0')
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return None;
        }
        Some(Self(relative))
    }

    /// Read callers must use this handle, never reopen the original pathname.
    #[cfg(unix)]
    pub(super) fn open_regular(&self, root: &Path) -> io::Result<File> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;

        // The registered root's ancestors are the trust anchor. Refuse a
        // replacement link at the root entry itself and pin the directory.
        let root: std::path::PathBuf = root.components().collect();
        let root = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: root is NUL terminated; a successful fd is owned by File.
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: open returned a new owned fd.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut components = self.0.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(part) = component else {
                return Err(io::ErrorKind::InvalidInput.into());
            };
            let name = CString::new(part.as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // Never wait on a replaced FIFO. Verify the type on the returned
            // descriptor rather than by another lookup of the path.
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if components.peek().is_some() {
                    libc::O_DIRECTORY
                } else {
                    0
                };
            // SAFETY: file is open, name is NUL terminated, and the returned
            // fd is transferred into File before the parent handle is dropped.
            let next = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
            if next < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openat returned a new owned fd.
            file = unsafe { File::from_raw_fd(next) };
        }
        if !file.metadata()?.is_file() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(file)
    }

    #[cfg(not(unix))]
    pub(super) fn open_regular(&self, _root: &Path) -> io::Result<File> {
        let _ = self.0;
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stored_path_grammar_keeps_dot_names_and_rejects_navigation() {
        for path in [
            "",
            ".",
            "../note.md",
            "sub/../note.md",
            "/note.md",
            "\\note.md",
            "a\0.md",
        ] {
            assert!(IndexedFilePath::new(path).is_none(), "{path:?}");
        }
        for path in ["dupe..name.md", "sub/.hidden.md", "Étage/note.md"] {
            assert!(IndexedFilePath::new(path).is_some(), "{path:?}");
        }
    }

    #[test]
    fn indexed_opener_refuses_replaced_root_and_nonregular_leaf() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let vault = temp.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(vault.join("note.md"), "ordinary note").unwrap();
        assert!(
            IndexedFilePath::new("note.md")
                .unwrap()
                .open_regular(&vault)
                .is_ok()
        );

        let replaced = temp.path().join("replaced");
        symlink(&vault, &replaced).unwrap();
        // A trailing separator must not hide a symlink at the root entry.
        let replaced = replaced.join("");
        assert!(
            IndexedFilePath::new("note.md")
                .unwrap()
                .open_regular(&replaced)
                .is_err()
        );

        let fifo = CString::new(vault.join("pipe.md").as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is a NUL-terminated name in the owned fixture.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        // Do not let a future loss of O_NONBLOCK hang the test runner.
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let refused = IndexedFilePath::new("pipe.md")
                .unwrap()
                .open_regular(&vault)
                .is_err();
            let _ = send.send(refused);
        });
        assert!(
            receive
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
    }
}
