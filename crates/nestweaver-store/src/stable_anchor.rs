//! Coordination anchors outside replaceable database/publication trees.
//!
//! Registry entries are never pruned while processes may be running. This is
//! cooperating-process exclusion, not protection from arbitrary same-UID writes:
//! the owner must not modify the registry (including via tmp cleaners). See
//! docs/architecture/filesystem-authority.md for the explicit trust boundary.

use std::fs::File;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct StableAnchor {
    file: File,
    path: PathBuf,
}

/// Match an open descriptor to a non-symlink pathname without opening another
/// descriptor (closing one could release a process-wide POSIX database lock).
pub fn descriptor_matches_path(file: &File, path: &Path) -> bool {
    let Ok(opened) = file.metadata() else {
        return false;
    };
    let Ok(named) = std::fs::symlink_metadata(path) else {
        return false;
    };
    !named.file_type().is_symlink() && opened.dev() == named.dev() && opened.ino() == named.ino()
}

impl StableAnchor {
    pub fn acquire(domain: &str, canonical_path: &Path, exclusive: bool) -> std::io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        // Fixed /tmp, never TMPDIR: different clients must choose the same
        // registry even when their launch environments differ.
        let uid = unsafe { libc::geteuid() };
        let root = PathBuf::from(format!("/tmp/nestweaver-authority-{uid}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(&root)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe NestWeaver authority registry: expected private owner directory",
            ));
        }
        let mut hash = blake3::Hasher::new();
        hash.update(domain.as_bytes());
        hash.update(&[0]);
        hash.update(canonical_path.as_os_str().as_bytes());
        let path = root.join(hash.finalize().to_hex().as_str());
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe NestWeaver authority registry entry",
            ));
        }
        for attempt in 0..=100 {
            let result = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match result {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if attempt < 100 => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => return Err(error.into()),
            }
        }
        let anchor = Self { file, path };
        if !anchor.is_current() {
            return Err(std::io::Error::other(
                "NestWeaver authority registry entry was replaced",
            ));
        }
        Ok(anchor)
    }

    pub fn is_current(&self) -> bool {
        descriptor_matches_path(&self.file, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_substitution_revokes_incumbent_authorization() {
        let scope = tempfile::tempdir().unwrap();
        let anchor = StableAnchor::acquire("test", scope.path(), true).unwrap();
        std::fs::rename(&anchor.path, anchor.path.with_extension("displaced")).unwrap();
        std::fs::write(&anchor.path, b"").unwrap();
        assert!(!anchor.is_current());
        // Replacing trusted registry state is outside the exclusion guarantee;
        // an incumbent can nevertheless detect it and must refuse mutation.
        std::fs::remove_file(anchor.path.with_extension("displaced")).unwrap();
    }
}
